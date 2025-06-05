//! When a peer is found using some discovery mechanisms (local discovery, DHT, PEX, ...), the
//! networking code will try to connect to it. However, if connecting to the peer fails we would
//! like to keep trying to connect to it again. On one hand we don't want to keep trying to connect
//! to the peer indefinitely, but on the other hand we also don't want to wait until the next time
//! the discovery mechanism finds the peer (which may be more than 10 minutes).
//!
//! This code solves the problem by giving the networking code a `SeenPeer` structure that
//! dereferences to `Some(PeerAddr)` for as long as the discovery mechanism "thinks" the peer
//! is still available, and to `None` once the mechanism hasn't seen the peer for a while.

use super::PeerAddr;
use crate::collections::{HashMap, HashSet};
use deadlock::BlockingRwLock;
use std::{fmt, sync::Arc};
use tokio::sync::watch;

// When a peer has not been seen after this many rounds, it'll be removed.
const REMOVE_AFTER_ROUND_COUNT: u64 = 2;

#[derive(Clone)]
pub(crate) struct SeenPeers {
    inner: Arc<BlockingRwLock<SeenPeersInner>>,
}

impl SeenPeers {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(BlockingRwLock::new(SeenPeersInner::new())),
        }
    }

    pub(crate) fn start_new_round(&self) {
        self.inner.write().unwrap().start_new_round()
    }

    pub(crate) fn insert(&self, peer: PeerAddr) -> Option<SeenPeer> {
        self.inner.write().unwrap().insert(peer, &self.inner)
    }

    pub(crate) fn remove(&self, peer: &PeerAddr) {
        self.inner.write().unwrap().remove(peer)
    }

    pub(crate) fn collect(&self) -> Vec<SeenPeer> {
        self.inner.write().unwrap().collect(&self.inner)
    }
}

type RoundId = u64;
type RefCount = usize;

struct SeenPeersInner {
    current_round_id: RoundId,
    peers: HashMap<PeerAddr, PeerEntry>,
    rounds: HashMap<RoundId, HashSet<PeerAddr>>,
}

impl SeenPeersInner {
    fn new() -> Self {
        Self {
            current_round_id: 0,
            peers: HashMap::default(),
            rounds: HashMap::default(),
        }
    }

    fn start_new_round(&mut self) {
        use std::collections::hash_map::Entry;

        self.current_round_id += 1;
        self.rounds.retain(|round, peers| {
            let is_old = round + REMOVE_AFTER_ROUND_COUNT < self.current_round_id;

            if is_old {
                for peer in peers.iter() {
                    let mut entry = match self.peers.entry(*peer) {
                        Entry::Occupied(entry) => entry,
                        Entry::Vacant(_) => unreachable!(),
                    };

                    let PeerEntry {
                        ref_count: rc,
                        rounds,
                        ..
                    } = entry.get_mut();
                    rounds.remove(round);

                    if *rc == 0 && rounds.is_empty() {
                        entry.remove();
                    }
                }
            }

            !is_old
        });
    }

    /// Returns `Some(SeenPeer)` if the peer has not been seen in the last
    /// REMOVE_AFTER_ROUND_COUNT rounds.
    fn insert(
        &mut self,
        addr: PeerAddr,
        ext: &Arc<BlockingRwLock<SeenPeersInner>>,
    ) -> Option<SeenPeer> {
        let round = self.rounds.entry(self.current_round_id).or_default();

        if !round.insert(addr) {
            // Already in current round
            return None;
        };

        let PeerEntry {
            ref_count: rc,
            rounds,
            is_seen_tx,
        } = self.peers.entry(addr).or_insert_with(|| PeerEntry {
            ref_count: 0,
            rounds: HashSet::default(),
            is_seen_tx: watch::channel(()).0,
        });

        let is_new = rounds.is_empty();

        // Assert because we checked above that it's not in `self.rounds`, so it must not have been
        // in `self.peers[addr].1` either.
        assert!(rounds.insert(self.current_round_id));

        if !is_new {
            // Already in one of the other rounds.
            return None;
        }

        *rc += 1;

        Some(SeenPeer {
            addr,
            seen_peers: ext.clone(),
            is_seen_rx: is_seen_tx.subscribe(),
        })
    }

    fn remove(&mut self, addr: &PeerAddr) {
        self.rounds.retain(|_round_id, peers| {
            peers.remove(addr);
            !peers.is_empty()
        });

        self.peers.remove(addr);
    }

    fn collect(&mut self, ext: &Arc<BlockingRwLock<SeenPeersInner>>) -> Vec<SeenPeer> {
        self.peers
            .iter_mut()
            .filter_map(
                |(
                    addr,
                    PeerEntry {
                        ref_count: rc,
                        rounds,
                        is_seen_tx,
                    },
                )| {
                    if rounds.is_empty() {
                        None
                    } else {
                        *rc += 1;
                        Some(SeenPeer {
                            addr: *addr,
                            seen_peers: ext.clone(),
                            is_seen_rx: is_seen_tx.subscribe(),
                        })
                    }
                },
            )
            .collect()
    }
}

struct PeerEntry {
    ref_count: RefCount,
    rounds: HashSet<RoundId>,
    // Droping this will cause `on_unseen` to complete.
    is_seen_tx: watch::Sender<()>,
}

pub(crate) struct SeenPeer {
    addr: PeerAddr,
    seen_peers: Arc<BlockingRwLock<SeenPeersInner>>,
    is_seen_rx: watch::Receiver<()>,
}

impl SeenPeer {
    pub(crate) fn initial_addr(&self) -> &PeerAddr {
        &self.addr
    }

    pub(crate) fn addr_if_seen(&self) -> Option<&PeerAddr> {
        let lock = self.seen_peers.read().unwrap();
        lock.peers
            .get(&self.addr)
            .and_then(|PeerEntry { rounds, .. }| {
                if rounds.is_empty() {
                    None
                } else {
                    Some(&self.addr)
                }
            })
    }

    pub(crate) async fn on_unseen(&self) {
        while self.is_seen_rx.clone().changed().await.is_ok() {}
    }
}

impl Clone for SeenPeer {
    fn clone(&self) -> Self {
        let mut seen_peers = self.seen_peers.write().unwrap();
        // Unwrap because if `self` exists, then there must be an entry in `peers` for it.
        seen_peers.peers.get_mut(&self.addr).unwrap().ref_count += 1;
        Self {
            addr: self.addr,
            seen_peers: self.seen_peers.clone(),
            is_seen_rx: self.is_seen_rx.clone(),
        }
    }
}

impl PartialEq for SeenPeer {
    fn eq(&self, other: &Self) -> bool {
        self.addr == other.addr && Arc::ptr_eq(&self.seen_peers, &other.seen_peers)
    }
}

impl Eq for SeenPeer {}

impl fmt::Debug for SeenPeer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SeenPeer")
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

impl Drop for SeenPeer {
    fn drop(&mut self) {
        use std::collections::hash_map::Entry;

        let mut seen_peers = self.seen_peers.write().unwrap();

        let mut peers_entry = match seen_peers.peers.entry(self.addr) {
            Entry::Occupied(entry) => entry,
            // Removed by the `SeenPeers::remove` function
            Entry::Vacant(_) => return,
        };

        let PeerEntry { ref_count: rc, .. } = peers_entry.get_mut();
        *rc -= 1;

        if *rc == 0 {
            let PeerEntry { rounds, .. } = peers_entry.remove();

            for round in rounds.iter() {
                let mut rounds_entry = match seen_peers.rounds.entry(*round) {
                    Entry::Occupied(entry) => entry,
                    Entry::Vacant(_) => unreachable!(),
                };
                let peers = rounds_entry.get_mut();
                assert!(peers.remove(&self.addr));
                if peers.is_empty() {
                    rounds_entry.remove();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn sanity_checks() {
        let seen_peers = SeenPeers::new();
        let peer_addr = PeerAddr::Quic((Ipv4Addr::LOCALHOST, 1234).into());
        let peer = seen_peers.insert(peer_addr).unwrap();

        assert!(seen_peers.insert(peer_addr).is_none());

        drop(peer);

        let peer = seen_peers.insert(peer_addr).unwrap();

        for _ in 0..(REMOVE_AFTER_ROUND_COUNT + 1) {
            assert!(peer.addr_if_seen().is_some());
            seen_peers.start_new_round();
        }

        assert!(peer.addr_if_seen().is_none());

        let peer = seen_peers.insert(peer_addr).unwrap();

        seen_peers.start_new_round();
        // Inserted, but it's not new, so None is returned.
        assert!(seen_peers.insert(peer_addr).is_none());

        seen_peers.start_new_round();
        assert!(peer.addr_if_seen().is_some());

        seen_peers.start_new_round();
        assert!(peer.addr_if_seen().is_some());

        seen_peers.start_new_round();
        assert!(peer.addr_if_seen().is_none());

        let _peer = seen_peers.insert(peer_addr).unwrap();
    }
}
