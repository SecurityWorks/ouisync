use super::BlockId;
use crate::collections::{hash_map::Entry, HashMap, HashSet};
use slab::Slab;
use std::{
    fmt,
    sync::{Arc, Mutex as BlockingMutex},
};
use tokio::sync::watch;

/// Helper for tracking required missing blocks.
#[derive(Clone)]
pub(crate) struct BlockTracker {
    shared: Arc<Shared>,
}

impl BlockTracker {
    pub fn new() -> Self {
        let (notify_tx, _) = watch::channel(());

        Self {
            shared: Arc::new(Shared {
                inner: BlockingMutex::new(Inner {
                    missing_blocks: HashMap::default(),
                    clients: Slab::new(),
                }),
                notify_tx,
            }),
        }
    }

    /// Begin marking the block with the given id as required. See also [`Require::commit`].
    pub fn begin_require(&self, block_id: BlockId) -> Require {
        let mut inner = self.shared.inner.lock().unwrap();

        inner
            .missing_blocks
            .entry(block_id)
            .or_insert_with(|| MissingBlock {
                clients: HashSet::default(),
                accepted_by: None,
                being_required: 0,
                required: 0,
            })
            .being_required += 1;

        Require {
            shared: self.shared.clone(),
            block_id,
        }
    }

    /// Mark the block request as successfully completed.
    pub fn complete(&self, block_id: &BlockId) {
        tracing::trace!(?block_id, "complete");

        let mut inner = self.shared.inner.lock().unwrap();

        let missing_block = if let Some(missing_block) = inner.missing_blocks.remove(block_id) {
            missing_block
        } else {
            return;
        };

        for client_id in missing_block.clients {
            if let Some(block_ids) = inner.clients.get_mut(client_id) {
                block_ids.remove(block_id);
            }
        }
    }

    pub fn client(&self) -> BlockTrackerClient {
        let client_id = self
            .shared
            .inner
            .lock()
            .unwrap()
            .clients
            .insert(HashSet::default());

        let notify_rx = self.shared.notify_tx.subscribe();

        BlockTrackerClient {
            shared: self.shared.clone(),
            client_id,
            notify_rx,
        }
    }

    #[cfg(test)]
    fn contains(&self, block_id: &BlockId) -> bool {
        self.shared
            .inner
            .lock()
            .unwrap()
            .missing_blocks
            .contains_key(block_id)
    }
}

pub(crate) struct BlockTrackerClient {
    shared: Arc<Shared>,
    client_id: ClientId,
    notify_rx: watch::Receiver<()>,
}

impl BlockTrackerClient {
    /// Offer to request the given block by the client with `client_id` if it is, or will become,
    /// required. Returns `true` if this block was offered for the first time (by any client), `false` if it was
    /// already offered before but not yet
    /// accepted or cancelled.
    pub fn offer(&self, block_id: BlockId) -> bool {
        let mut inner = self.shared.inner.lock().unwrap();

        if !inner.clients[self.client_id].insert(block_id) {
            // Already offered
            return false;
        }

        tracing::trace!(?block_id, "offer");

        let missing_block = inner
            .missing_blocks
            .entry(block_id)
            .or_insert_with(|| MissingBlock {
                clients: HashSet::default(),
                accepted_by: None,
                being_required: 0,
                required: 0,
            });

        missing_block.clients.insert(self.client_id);

        true
    }

    /// Cancel a previously accepted request so it can be attempted by another client.
    pub fn cancel(&self, block_id: &BlockId) {
        let mut inner = self.shared.inner.lock().unwrap();

        if !inner.clients[self.client_id].remove(block_id) {
            return;
        }

        tracing::trace!(?block_id, "cancel");

        // unwrap is ok because of the invariant in `Inner`
        let missing_block = inner.missing_blocks.get_mut(block_id).unwrap();
        missing_block.clients.remove(&self.client_id);

        if missing_block.unaccept_by(self.client_id) {
            self.shared.notify();
        }
    }

    /// Returns the next required and offered block request. If there is no such request at the
    /// moment this function is called, waits until one appears.
    ///
    /// # Cancel safety
    ///
    /// This method is cancel safe.
    pub async fn accept_(&mut self) -> AcceptedBlock {
        loop {
            if let Some(accepted_block) = self.try_accept_() {
                return accepted_block;
            }

            // unwrap is ok because the sender exists in self.shared.
            self.notify_rx.changed().await.unwrap();
        }
    }

    /// Returns the next required and offered block request or `None` if there is no such request
    /// currently.
    pub fn try_accept_(&self) -> Option<AcceptedBlock> {
        let mut inner = self.shared.inner.lock().unwrap();
        let inner = &mut *inner;

        // TODO: OPTIMIZE (but profile first) this linear lookup
        for block_id in &inner.clients[self.client_id] {
            // unwrap is ok because of the invariant in `Inner`
            let missing_block = inner.missing_blocks.get_mut(block_id).unwrap();

            if missing_block.required > 0 && missing_block.accepted_by.is_none() {
                missing_block.accepted_by = Some(self.client_id);
                return Some(AcceptedBlock {
                    shared: self.shared.clone(),
                    client_id: self.client_id,
                    block_id: *block_id,
                });
            }
        }

        None
    }

    /// Returns the next required and offered block request. If there is no such request at the
    /// moment this function is called, waits until one appears.
    ///
    /// # Cancel safety
    ///
    /// This method is cancel safe.
    pub async fn accept(&mut self) -> BlockId {
        loop {
            if let Some(block_id) = self.try_accept() {
                return block_id;
            }

            // unwrap is ok because the sender exists in self.shared.
            self.notify_rx.changed().await.unwrap();
        }
    }

    /// Returns the next required and offered block request or `None` if there is no such request
    /// currently.
    pub fn try_accept(&self) -> Option<BlockId> {
        let mut inner = self.shared.inner.lock().unwrap();
        let inner = &mut *inner;

        // TODO: OPTIMIZE (but profile first) this linear lookup
        for block_id in &inner.clients[self.client_id] {
            // unwrap is ok because of the invariant in `Inner`
            let missing_block = inner.missing_blocks.get_mut(block_id).unwrap();

            if missing_block.required > 0 && missing_block.accepted_by.is_none() {
                missing_block.accepted_by = Some(self.client_id);
                return Some(*block_id);
            }
        }

        None
    }
}

pub(crate) struct AcceptedBlock {
    shared: Arc<Shared>,
    client_id: ClientId,
    block_id: BlockId,
}

impl AcceptedBlock {
    pub(crate) fn block_id(&self) -> &BlockId {
        &self.block_id
    }

    pub fn complete(&self) {
        tracing::trace!(?self.block_id, "complete");

        let mut inner = self.shared.inner.lock().unwrap();

        let missing_block = if let Some(missing_block) = inner.missing_blocks.remove(&self.block_id)
        {
            missing_block
        } else {
            return;
        };

        for client_id in missing_block.clients {
            if let Some(block_ids) = inner.clients.get_mut(client_id) {
                block_ids.remove(&self.block_id);
            }
        }
    }

    pub(crate) fn cancel(&self) {
        self.cancel_with_reason("cancel");
    }

    fn cancel_with_reason(&self, reason: &'static str) {
        let mut inner = self.shared.inner.lock().unwrap();

        let client = match inner.clients.get_mut(self.client_id) {
            Some(client) => client,
            None => return,
        };

        if !client.remove(&self.block_id) {
            return;
        }

        tracing::trace!(?self.block_id, reason);

        // unwrap is ok because of the invariant in `Inner`
        let missing_block = inner.missing_blocks.get_mut(&self.block_id).unwrap();
        missing_block.clients.remove(&self.client_id);

        if missing_block.unaccept_by(self.client_id) {
            self.shared.notify();
        }
    }
}

impl fmt::Debug for AcceptedBlock {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("AcceptedBlock")
            .field("client_id", &self.client_id)
            .field("block_id", &self.block_id)
            .finish()
    }
}

impl Drop for AcceptedBlock {
    fn drop(&mut self) {
        self.cancel_with_reason("drop");
    }
}

impl Drop for BlockTrackerClient {
    fn drop(&mut self) {
        let mut inner = self.shared.inner.lock().unwrap();
        let block_ids = inner.clients.remove(self.client_id);
        let mut notify = false;

        for block_id in block_ids {
            // unwrap is ok because of the invariant in `Inner`
            let missing_block = inner.missing_blocks.get_mut(&block_id).unwrap();

            missing_block.clients.remove(&self.client_id);

            if missing_block.unaccept_by(self.client_id) {
                notify = true;
            }
        }

        if notify {
            self.shared.notify()
        }
    }
}

pub(crate) struct Require {
    shared: Arc<Shared>,
    block_id: BlockId,
}

impl Require {
    pub fn commit(self) {
        let mut inner = self.shared.inner.lock().unwrap();

        match inner.missing_blocks.entry(self.block_id) {
            Entry::Occupied(mut entry) => {
                let missing_block = entry.get_mut();

                if missing_block.required == 0 {
                    tracing::trace!(block_id = ?self.block_id, "require");
                    self.shared.notify();
                }

                missing_block.required += 1;
            }
            Entry::Vacant(_) => return,
        };
    }
}

impl Drop for Require {
    fn drop(&mut self) {
        let mut inner = self.shared.inner.lock().unwrap();
        let mut offered_by = Default::default();

        match inner.missing_blocks.entry(self.block_id) {
            Entry::Occupied(mut entry) => {
                let missing_block = entry.get_mut();
                missing_block.being_required -= 1;
                if missing_block.being_required == 0 && missing_block.required == 0 {
                    std::mem::swap(&mut offered_by, &mut missing_block.clients);
                    entry.remove();
                }
            }
            Entry::Vacant(_) => {}
        }

        for client_id in offered_by {
            inner.clients[client_id].remove(&self.block_id);
        }
    }
}

struct Shared {
    inner: BlockingMutex<Inner>,
    notify_tx: watch::Sender<()>,
}

impl Shared {
    fn notify(&self) {
        self.notify_tx.send(()).unwrap_or(())
    }
}

// Invariant: for all `block_id` and `client_id` such that
//
//     missing_blocks[block_id].clients.contains(client_id)
//
// it must hold that
//
//     clients[client_id].contains(block_id)
//
// and vice-versa.
struct Inner {
    missing_blocks: HashMap<BlockId, MissingBlock>,
    clients: Slab<HashSet<BlockId>>,
}

#[derive(Debug)]
struct MissingBlock {
    clients: HashSet<ClientId>,
    accepted_by: Option<ClientId>,
    being_required: usize,
    required: usize,
}

impl MissingBlock {
    fn unaccept_by(&mut self, client_id: ClientId) -> bool {
        if let Some(accepted_by) = &self.accepted_by {
            if accepted_by == &client_id {
                self.accepted_by = None;
                return true;
            }
        }

        return false;
    }
}

type ClientId = usize;

#[cfg(test)]
mod tests {
    use super::{
        super::{BlockData, BLOCK_SIZE},
        *,
    };
    use crate::{collections::HashSet, test_utils};
    use futures_util::future;
    use rand::{distributions::Standard, rngs::StdRng, seq::SliceRandom, Rng, SeedableRng};
    use test_strategy::proptest;
    use tokio::{sync::Barrier, task};

    #[test]
    fn simple() {
        let tracker = BlockTracker::new();

        let client = tracker.client();

        // Initially no blocks are returned
        assert_eq!(client.try_accept(), None);

        // Offered but not required blocks are not returned
        let block0 = make_block();
        client.offer(block0.id);
        assert_eq!(client.try_accept(), None);

        // Required but not offered blocks are not returned
        let block1 = make_block();
        tracker.begin_require(block1.id).commit();
        assert_eq!(client.try_accept(), None);

        // Required + offered blocks are returned...
        tracker.begin_require(block0.id).commit();
        assert_eq!(client.try_accept(), Some(block0.id));

        // ...but only once.
        assert_eq!(client.try_accept(), None);
    }

    #[test]
    fn fallback_on_cancel_before_accept() {
        let tracker = BlockTracker::new();

        let client0 = tracker.client();
        let client1 = tracker.client();

        let block = make_block();

        tracker.begin_require(block.id).commit();
        client0.offer(block.id);
        client1.offer(block.id);

        client0.cancel(&block.id);

        assert_eq!(client0.try_accept(), None);
        assert_eq!(client1.try_accept(), Some(block.id));

        tracker.complete(&block.id);

        assert!(tracker
            .shared
            .inner
            .lock()
            .unwrap()
            .missing_blocks
            .is_empty());
    }

    #[test]
    fn fallback_on_cancel_before_accept_() {
        let tracker = BlockTracker::new();

        let client0 = tracker.client();
        let client1 = tracker.client();

        let block = make_block();

        tracker.begin_require(block.id).commit();
        client0.offer(block.id);
        client1.offer(block.id);

        client0.cancel(&block.id);

        assert_eq!(client0.try_accept(), None);

        let accepted_block = client1.try_accept_().unwrap();
        assert_eq!(accepted_block.block_id(), &block.id);

        accepted_block.complete();

        assert!(tracker
            .shared
            .inner
            .lock()
            .unwrap()
            .missing_blocks
            .is_empty());
    }

    #[test]
    fn fallback_on_cancel_after_accept() {
        let tracker = BlockTracker::new();

        let client0 = tracker.client();
        let client1 = tracker.client();

        let block = make_block();

        tracker.begin_require(block.id).commit();
        client0.offer(block.id);
        client1.offer(block.id);

        assert_eq!(client0.try_accept(), Some(block.id));
        assert_eq!(client1.try_accept(), None);

        client0.cancel(&block.id);

        assert_eq!(client0.try_accept(), None);
        assert_eq!(client1.try_accept(), Some(block.id));
    }

    #[test]
    fn fallback_on_client_drop_after_require_before_accept() {
        let tracker = BlockTracker::new();

        let client0 = tracker.client();
        let client1 = tracker.client();

        let block = make_block();

        client0.offer(block.id);
        client1.offer(block.id);

        tracker.begin_require(block.id).commit();

        drop(client0);

        assert_eq!(client1.try_accept(), Some(block.id));
    }

    #[test]
    fn fallback_on_client_drop_after_require_after_accept() {
        let tracker = BlockTracker::new();

        let client0 = tracker.client();
        let client1 = tracker.client();

        let block = make_block();

        client0.offer(block.id);
        client1.offer(block.id);

        tracker.begin_require(block.id).commit();

        assert_eq!(client0.try_accept(), Some(block.id));
        assert_eq!(client1.try_accept(), None);

        drop(client0);

        assert_eq!(client1.try_accept(), Some(block.id));
    }

    #[test]
    fn fallback_on_client_drop_before_request() {
        let tracker = BlockTracker::new();

        let client0 = tracker.client();
        let client1 = tracker.client();

        let block = make_block();

        client0.offer(block.id);
        client1.offer(block.id);

        drop(client0);

        tracker.begin_require(block.id).commit();

        assert_eq!(client1.try_accept(), Some(block.id));
    }

    #[test]
    fn concurrent_require_and_complete() {
        let tracker = BlockTracker::new();
        let client = tracker.client();

        let block = make_block();
        client.offer(block.id);

        let require = tracker.begin_require(block.id);
        tracker.complete(&block.id);
        require.commit();

        assert!(!tracker.contains(&block.id));
    }

    #[test]
    fn concurrent_require_and_drop() {
        let tracker = BlockTracker::new();
        let client = tracker.client();

        let block = make_block();
        client.offer(block.id);

        let require1 = tracker.begin_require(block.id);
        let require2 = tracker.begin_require(block.id);
        drop(require1);
        require2.commit();

        assert_eq!(client.try_accept(), Some(block.id));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn race() {
        let num_clients = 10;

        let tracker = BlockTracker::new();
        let clients: Vec<_> = (0..num_clients).map(|_| tracker.client()).collect();

        let block = make_block();

        tracker.begin_require(block.id).commit();

        for client in &clients {
            client.offer(block.id);
        }

        // Make sure all clients stay alive until we are done so that any accepted requests are not
        // released prematurely.
        let barrier = Arc::new(Barrier::new(clients.len()));

        // Run the clients in parallel
        let handles = clients.into_iter().map(|client| {
            task::spawn({
                let barrier = barrier.clone();
                async move {
                    let result = client.try_accept();
                    barrier.wait().await;
                    result
                }
            })
        });

        let block_ids = future::try_join_all(handles).await.unwrap();

        // Exactly one client gets the block id
        let mut block_ids = block_ids.into_iter().flatten();
        assert_eq!(block_ids.next(), Some(block.id));
        assert_eq!(block_ids.next(), None);
    }

    #[proptest]
    fn stress(
        #[strategy(1usize..100)] num_blocks: usize,
        #[strategy(test_utils::rng_seed_strategy())] rng_seed: u64,
    ) {
        stress_case(num_blocks, rng_seed)
    }

    fn stress_case(num_blocks: usize, rng_seed: u64) {
        let mut rng = StdRng::seed_from_u64(rng_seed);

        let tracker = BlockTracker::new();
        let client = tracker.client();

        let block_ids: Vec<BlockId> = (&mut rng).sample_iter(Standard).take(num_blocks).collect();

        enum Op {
            Require,
            Offer,
        }

        let mut ops: Vec<_> = block_ids
            .iter()
            .map(|block_id| (Op::Require, *block_id))
            .chain(block_ids.iter().map(|block_id| (Op::Offer, *block_id)))
            .collect();
        ops.shuffle(&mut rng);

        for (op, block_id) in ops {
            match op {
                Op::Require => {
                    tracker.begin_require(block_id).commit();
                }
                Op::Offer => {
                    client.offer(block_id);
                }
            }
        }

        let mut accepted_block_ids = HashSet::with_capacity(block_ids.len());

        while let Some(block_id) = client.try_accept() {
            accepted_block_ids.insert(block_id);
        }

        assert_eq!(accepted_block_ids.len(), block_ids.len());

        for block_id in &block_ids {
            assert!(accepted_block_ids.contains(block_id));
        }
    }

    fn make_block() -> BlockData {
        let mut content = vec![0; BLOCK_SIZE].into_boxed_slice();
        rand::thread_rng().fill(&mut content[..]);

        BlockData::from(content)
    }
}
