use super::{
    debug_payload::{DebugReceivedResponse, DebugRequest},
    message::{Request, Response, ResponseDisambiguator},
};
use crate::{
    block::{tracker::BlockPromise, BlockData, BlockId, BlockNonce},
    collections::{hash_map::Entry, HashMap},
    crypto::{sign::PublicKey, CacheHash, Hash, Hashable},
    index::{InnerNodeMap, LeafNodeSet, Summary, UntrustedProof},
    repository_stats::{self, RepositoryStats},
    sync::uninitialized_watch,
};
use scoped_task::ScopedJoinHandle;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::{select, sync::OwnedSemaphorePermit, time};

// If a response to a pending request is not received within this time, a request timeout error is
// triggered.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub(crate) enum PendingRequest {
    RootNode(PublicKey, DebugRequest),
    ChildNodes(Hash, ResponseDisambiguator, DebugRequest),
    Block(BlockPromise, DebugRequest),
}

impl PendingRequest {
    pub fn to_key(&self) -> Key {
        // Debug payloads are ignored in keys.
        match self {
            Self::RootNode(public_key, _) => Key::RootNode(*public_key),
            Self::ChildNodes(hash, disambiguator, _) => Key::ChildNodes(*hash, *disambiguator),
            Self::Block(block_promise, _) => Key::Block(*block_promise.block_id()),
        }
    }

    pub fn to_message(&self) -> Request {
        match self {
            Self::RootNode(public_key, debug) => Request::RootNode(*public_key, debug.send()),
            Self::ChildNodes(hash, disambiguator, debug) => {
                Request::ChildNodes(*hash, *disambiguator, debug.send())
            }
            Self::Block(block_promise, debug) => {
                Request::Block(*block_promise.block_id(), debug.send())
            }
        }
    }
}

pub(super) enum PendingResponse {
    RootNode {
        proof: UntrustedProof,
        summary: Summary,
        permit: Option<OwnedSemaphorePermit>,
        debug: DebugReceivedResponse,
    },
    InnerNodes {
        hash: CacheHash<InnerNodeMap>,
        permit: Option<OwnedSemaphorePermit>,
        debug: DebugReceivedResponse,
    },
    LeafNodes {
        hash: CacheHash<LeafNodeSet>,
        permit: Option<OwnedSemaphorePermit>,
        debug: DebugReceivedResponse,
    },
    Block {
        data: BlockData,
        nonce: BlockNonce,
        block_promise: Option<BlockPromise>,
        permit: Option<OwnedSemaphorePermit>,
        debug: DebugReceivedResponse,
    },
}

pub(super) struct PendingRequests {
    stats: Arc<RepositoryStats>,
    map: Arc<Mutex<HashMap<Key, RequestData>>>,
    to_tracker_tx: uninitialized_watch::Sender<()>,
    _expiration_tracker: ScopedJoinHandle<()>,
}

impl PendingRequests {
    pub fn new(stats: Arc<RepositoryStats>) -> Self {
        let map = Arc::new(Mutex::new(HashMap::<Key, RequestData>::default()));

        let (expiration_tracker, to_tracker_tx) = run_tracker(stats.clone(), map.clone());

        Self {
            stats,
            map,
            to_tracker_tx,
            _expiration_tracker: expiration_tracker,
        }
    }

    pub fn insert(&self, pending_request: PendingRequest, permit: CompoundPermit) -> bool {
        match self.map.lock().unwrap().entry(pending_request.to_key()) {
            Entry::Occupied(_) => false,
            Entry::Vacant(entry) => {
                let msg = pending_request.to_key();

                let block_promise = match pending_request {
                    PendingRequest::RootNode(_, _) => None,
                    PendingRequest::ChildNodes(_, _, _) => None,
                    PendingRequest::Block(block_promise, _) => Some(block_promise),
                };

                entry.insert(RequestData {
                    timestamp: Instant::now(),
                    block_promise,
                    permit,
                });
                self.request_added(&msg);
                true
            }
        }
    }

    pub fn remove(&self, response: Response) -> Option<PendingResponse> {
        let response = ProcessedResponse::from(response);
        let key = response.to_key();

        if let Some(request_data) = self.map.lock().unwrap().remove(&key) {
            self.request_removed(&key, Some(request_data.timestamp));

            // We `drop` the `peer_permit` here but the `Client` will need the `client_permit` and
            // only `drop` it once the request is processed.
            let permit = Some(request_data.permit.client_permit);

            match response {
                ProcessedResponse::Success(success) => {
                    let r = match success {
                        processed_response::Success::RootNode {
                            proof,
                            summary,
                            debug,
                        } => PendingResponse::RootNode {
                            proof,
                            summary,
                            permit,
                            debug,
                        },
                        processed_response::Success::InnerNodes(hash, _disambiguator, debug) => {
                            PendingResponse::InnerNodes {
                                hash,
                                permit,
                                debug,
                            }
                        }
                        processed_response::Success::LeafNodes(hash, _disambiguator, debug) => {
                            PendingResponse::LeafNodes {
                                hash,
                                permit,
                                debug,
                            }
                        }
                        processed_response::Success::Block { data, nonce, debug } => {
                            PendingResponse::Block {
                                data,
                                nonce,
                                permit,
                                debug,
                                block_promise: request_data.block_promise,
                            }
                        }
                    };
                    Some(r)
                }
                ProcessedResponse::Failure(_) => None,
            }
        } else {
            // Only `RootNode` response is allowed to be unsolicited
            match response {
                ProcessedResponse::Success(processed_response::Success::RootNode {
                    proof,
                    summary,
                    debug,
                }) => Some(PendingResponse::RootNode {
                    proof,
                    summary,
                    permit: None,
                    debug,
                }),
                _ => None,
            }
        }
    }

    fn request_added(&self, key: &Key) {
        stats_request_added(&mut self.stats.write(), key);
        self.notify_tracker_task();
    }

    fn request_removed(&self, key: &Key, timestamp: Option<Instant>) {
        stats_request_removed(&mut self.stats.write(), key, timestamp);
        self.notify_tracker_task();
    }

    fn notify_tracker_task(&self) {
        self.to_tracker_tx.send(()).unwrap_or(());
    }
}

fn stats_request_added(stats: &mut repository_stats::Writer, key: &Key) {
    match key {
        Key::RootNode(_) | Key::ChildNodes { .. } => stats.index_requests_inflight += 1,
        Key::Block(_) => stats.block_requests_inflight += 1,
    }
}

fn stats_request_removed(
    stats: &mut repository_stats::Writer,
    key: &Key,
    timestamp: Option<Instant>,
) {
    match key {
        Key::RootNode(_) | Key::ChildNodes { .. } => stats.index_requests_inflight -= 1,
        Key::Block(_) => stats.block_requests_inflight -= 1,
    }
    if let Some(timestamp) = timestamp {
        stats.note_request_inflight_duration(Instant::now() - timestamp);
    }
}

fn run_tracker(
    stats: Arc<RepositoryStats>,
    request_map: Arc<Mutex<HashMap<Key, RequestData>>>,
) -> (ScopedJoinHandle<()>, uninitialized_watch::Sender<()>) {
    let (to_tracker_tx, mut to_tracker_rx) = uninitialized_watch::channel::<()>();

    let expiration_tracker = scoped_task::spawn(async move {
        loop {
            let entry = request_map
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, data)| data.block_promise.is_some())
                .min_by(|(_, lhs), (_, rhs)| lhs.timestamp.cmp(&rhs.timestamp))
                .map(|(k, v)| (*k, v.timestamp));

            if let Some((key, timestamp)) = entry {
                select! {
                    r = to_tracker_rx.changed() => {
                        match r {
                            Ok(()) => continue,
                            Err(_) => break,
                        }
                    }
                    _ = time::sleep_until((timestamp + REQUEST_TIMEOUT).into()) => {
                        // Check it hasn't been removed in a meanwhile for cancel safety.
                        if let Some(mut data) = request_map.lock().unwrap().get_mut(&key) {
                            stats.write().request_timeouts += 1;
                            data.block_promise = None;
                        }
                    }
                };
            } else {
                match to_tracker_rx.changed().await {
                    Ok(()) => continue,
                    Err(_) => break,
                }
            }
        }
    });

    (expiration_tracker, to_tracker_tx)
}

impl Drop for PendingRequests {
    fn drop(&mut self) {
        for key in self.map.lock().unwrap().keys() {
            self.request_removed(key, None);
        }
    }
}

struct RequestData {
    timestamp: Instant,
    block_promise: Option<BlockPromise>,
    permit: CompoundPermit,
}

// When sending requests, we need to limit it in two ways:
//
// 1. Limit how many requests we send to the peer across all repositories, and
// 2. Limit sending requests from a Client if too many responses are queued up.
pub(super) struct CompoundPermit {
    pub _peer_permit: OwnedSemaphorePermit,
    pub client_permit: OwnedSemaphorePermit,
}

mod processed_response {
    use super::*;

    pub(super) enum Success {
        RootNode {
            proof: UntrustedProof,
            summary: Summary,
            debug: DebugReceivedResponse,
        },
        InnerNodes(
            CacheHash<InnerNodeMap>,
            ResponseDisambiguator,
            DebugReceivedResponse,
        ),
        LeafNodes(
            CacheHash<LeafNodeSet>,
            ResponseDisambiguator,
            DebugReceivedResponse,
        ),
        Block {
            data: BlockData,
            nonce: BlockNonce,
            debug: DebugReceivedResponse,
        },
    }

    #[derive(Debug)]
    pub(super) enum Failure {
        RootNode(PublicKey, DebugReceivedResponse),
        ChildNodes(Hash, ResponseDisambiguator, DebugReceivedResponse),
        Block(BlockId, DebugReceivedResponse),
    }
}

enum ProcessedResponse {
    Success(processed_response::Success),
    Failure(processed_response::Failure),
}

impl ProcessedResponse {
    fn to_key(&self) -> Key {
        match self {
            Self::Success(processed_response::Success::RootNode { proof, .. }) => {
                Key::RootNode(proof.writer_id)
            }
            Self::Success(processed_response::Success::InnerNodes(nodes, disambiguator, _)) => {
                Key::ChildNodes(nodes.hash(), *disambiguator)
            }
            Self::Success(processed_response::Success::LeafNodes(nodes, disambiguator, _)) => {
                Key::ChildNodes(nodes.hash(), *disambiguator)
            }
            Self::Success(processed_response::Success::Block { data, .. }) => Key::Block(data.id),
            Self::Failure(processed_response::Failure::RootNode(branch_id, _)) => {
                Key::RootNode(*branch_id)
            }
            Self::Failure(processed_response::Failure::ChildNodes(hash, disambiguator, _)) => {
                Key::ChildNodes(*hash, *disambiguator)
            }
            Self::Failure(processed_response::Failure::Block(block_id, _)) => Key::Block(*block_id),
        }
    }
}

impl From<Response> for ProcessedResponse {
    fn from(response: Response) -> Self {
        match response {
            Response::RootNode {
                proof,
                summary,
                debug,
            } => Self::Success(processed_response::Success::RootNode {
                proof,
                summary,
                debug: debug.received(),
            }),
            Response::InnerNodes(nodes, disambiguator, debug) => {
                Self::Success(processed_response::Success::InnerNodes(
                    nodes.into(),
                    disambiguator,
                    debug.received(),
                ))
            }
            Response::LeafNodes(nodes, disambiguator, debug) => {
                Self::Success(processed_response::Success::LeafNodes(
                    nodes.into(),
                    disambiguator,
                    debug.received(),
                ))
            }
            Response::Block {
                content,
                nonce,
                debug,
            } => Self::Success(processed_response::Success::Block {
                data: content.into(),
                nonce,
                debug: debug.received(),
            }),
            Response::RootNodeError(branch_id, debug) => Self::Failure(
                processed_response::Failure::RootNode(branch_id, debug.received()),
            ),
            Response::ChildNodesError(hash, disambiguator, debug) => Self::Failure(
                processed_response::Failure::ChildNodes(hash, disambiguator, debug.received()),
            ),
            Response::BlockError(block_id, debug) => Self::Failure(
                processed_response::Failure::Block(block_id, debug.received()),
            ),
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq, Hash, Debug)]
pub(crate) enum Key {
    RootNode(PublicKey),
    ChildNodes(Hash, ResponseDisambiguator),
    Block(BlockId),
}
