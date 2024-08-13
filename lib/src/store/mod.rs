mod block;
mod block_expiration_tracker;
mod block_ids;
mod cache;
mod changeset;
mod client;
mod error;
mod index;
mod inner_node;
mod leaf_node;
mod migrations;
mod misc;
mod patch;
mod quota;
mod root_node;

#[cfg(test)]
mod test_utils;
#[cfg(test)]
mod tests;

pub use error::Error;
pub use migrations::DATA_VERSION;

pub(crate) use {block_ids::BlockIdsPage, changeset::Changeset, client::ClientWriter};

#[cfg(test)]
pub(crate) use test_utils::SnapshotWriter;

use self::{
    block_expiration_tracker::BlockExpirationTracker,
    cache::{Cache, CacheTransaction},
};
use crate::{
    block_tracker::BlockTracker as BlockDownloadTracker,
    crypto::{
        sign::{Keypair, PublicKey},
        Hash,
    },
    db,
    debug::DebugPrinter,
    progress::Progress,
    protocol::{
        get_bucket, BlockContent, BlockId, BlockNonce, InnerNodes, LeafNodes, RootNode,
        RootNodeFilter, INNER_LAYER_COUNT,
    },
    sync::broadcast_hash_set,
};
use futures_util::{Stream, TryStreamExt};
use std::{
    borrow::Cow,
    ops::{Deref, DerefMut},
    path::Path,
    sync::Arc,
    time::Duration,
};
// TODO: Consider creating an async `RwLock` in the `deadlock` module and use it here.
use tokio::sync::RwLock;

/// Data store
#[derive(Clone)]
pub(crate) struct Store {
    db: db::Pool,
    cache: Arc<Cache>,
    pub client_reload_index_tx: broadcast_hash_set::Sender<PublicKey>,
    block_expiration_tracker: Arc<RwLock<Option<Arc<BlockExpirationTracker>>>>,
}

impl Store {
    pub fn new(db: db::Pool) -> Self {
        let client_reload_index_tx = broadcast_hash_set::channel().0;

        Self {
            db,
            cache: Arc::new(Cache::new()),
            client_reload_index_tx,
            block_expiration_tracker: Arc::new(RwLock::new(None)),
        }
    }

    /// Runs data migrations. Does nothing if already at the latest version.
    pub async fn migrate_data(
        &self,
        this_writer_id: PublicKey,
        write_keys: &Keypair,
    ) -> Result<(), Error> {
        migrations::run_data(self, this_writer_id, write_keys).await
    }

    /// Check data integrity
    pub async fn check_integrity(&self) -> Result<bool, Error> {
        misc::check_integrity(self.acquire_read().await?.db()).await
    }

    pub async fn set_block_expiration(
        &self,
        expiration_time: Option<Duration>,
        block_download_tracker: BlockDownloadTracker,
    ) -> Result<(), Error> {
        let mut tracker_lock = self.block_expiration_tracker.write().await;

        if let Some(tracker) = &*tracker_lock {
            if let Some(expiration_time) = expiration_time {
                tracker.set_expiration_time(expiration_time);
            }
            return Ok(());
        }

        let expiration_time = match expiration_time {
            Some(expiration_time) => expiration_time,
            // Tracker is `None` so we're good.
            None => return Ok(()),
        };

        let tracker = BlockExpirationTracker::enable_expiration(
            self.db.clone(),
            expiration_time,
            block_download_tracker,
            self.client_reload_index_tx.clone(),
            self.cache.clone(),
        )
        .await?;

        *tracker_lock = Some(Arc::new(tracker));

        Ok(())
    }

    pub async fn block_expiration(&self) -> Option<Duration> {
        self.block_expiration_tracker
            .read()
            .await
            .as_ref()
            .map(|tracker| tracker.block_expiration())
    }

    #[cfg(test)]
    pub async fn block_expiration_tracker(&self) -> Option<Arc<BlockExpirationTracker>> {
        self.block_expiration_tracker.read().await.as_ref().cloned()
    }

    /// Export the whole repository db to the given file.
    pub async fn export(&self, dst: &Path) -> Result<(), Error> {
        misc::export(&mut *self.db.acquire().await?, dst).await
    }

    /// Acquires a `Reader`
    pub async fn acquire_read(&self) -> Result<Reader, Error> {
        Ok(Reader {
            inner: Handle::Connection(self.db.acquire().await?),
            cache: self.cache.begin(),
            block_expiration_tracker: self.block_expiration_tracker.read().await.clone(),
        })
    }

    /// Begins a `ReadTransaction`
    pub async fn begin_read(&self) -> Result<ReadTransaction, Error> {
        Ok(ReadTransaction {
            inner: Reader {
                inner: Handle::ReadTransaction(self.db.begin_read().await?),
                cache: self.cache.begin(),
                block_expiration_tracker: self.block_expiration_tracker.read().await.clone(),
            },
        })
    }

    /// Begins a `WriteTransaction`
    pub async fn begin_write(&self) -> Result<WriteTransaction, Error> {
        Ok(WriteTransaction {
            inner: ReadTransaction {
                inner: Reader {
                    inner: Handle::WriteTransaction(self.db.begin_write().await?),
                    cache: self.cache.begin(),
                    block_expiration_tracker: self.block_expiration_tracker.read().await.clone(),
                },
            },
            untrack_blocks: None,
        })
    }

    pub async fn begin_client_write(&self) -> Result<ClientWriter, Error> {
        ClientWriter::begin(
            self.db().begin_write().await?,
            self.cache.begin(),
            self.block_expiration_tracker.read().await.clone(),
        )
        .await
    }

    pub async fn count_blocks(&self) -> Result<u64, Error> {
        self.acquire_read().await?.count_blocks().await
    }

    /// Retrieve the syncing progress of this repository (number of present blocks / number of all
    /// blocks)
    pub async fn sync_progress(&self) -> Result<Progress, Error> {
        let mut reader = self.acquire_read().await?;

        let total = reader.count_block_ids().await?;
        let present = reader.count_blocks().await?;

        Ok(Progress {
            value: present,
            total,
        })
    }

    /// Remove outdated older snapshots.
    ///
    /// This preserves older snapshots that can be used as fallback for the latest snapshot and only
    /// removes those that can't. This also preserves all older snapshots that have the same
    /// version vector as the latest one (that is, when the latest snapshot is a draft).
    pub async fn remove_outdated_snapshots(&self, root_node: &RootNode) -> Result<(), Error> {
        // First remove all incomplete snapshots as they can never serve as fallback.
        let mut tx = self.begin_write().await?;
        root_node::remove_older_incomplete(tx.db(), root_node).await?;
        tx.commit().await?;

        let mut reader = self.acquire_read().await?;

        // Then remove those snapshots that can't serve as fallback for the current one.
        let mut new = Cow::Borrowed(root_node);

        while let Some(old) = reader.load_prev_approved_root_node(&new).await? {
            if old.proof.version_vector == new.proof.version_vector {
                // `new` is a draft and so we can't remove `old`. Try the previous snapshot.
                tracing::trace!(
                    branch_id = ?old.proof.writer_id,
                    hash = ?old.proof.hash,
                    vv = ?old.proof.version_vector,
                    "outdated snapshot not removed - draft"
                );

                new = Cow::Owned(old);
                continue;
            }

            if root_node::check_fallback(reader.db(), &old, &new).await? {
                // `old` can serve as fallback for `self` and so we can't prune it yet. Try the
                // previous snapshot.
                tracing::trace!(
                    branch_id = ?old.proof.writer_id,
                    hash = ?old.proof.hash,
                    vv = ?old.proof.version_vector,
                    "outdated snapshot not removed - possible fallback"
                );

                new = Cow::Owned(old);
                continue;
            }

            // `old` can't serve as fallback for `self` and so we can safely remove it
            let mut tx = self.begin_write().await?;
            root_node::remove(tx.db(), &old).await?;
            tx.commit().await?;

            tracing::trace!(
                branch_id = ?old.proof.writer_id,
                hash = ?old.proof.hash,
                vv = ?old.proof.version_vector,
                "outdated snapshot removed"
            );
        }

        Ok(())
    }

    /// Returns all block ids referenced from complete snapshots. The result is paginated (with
    /// `page_size` entries per page) to avoid loading too many items into memory.
    pub fn block_ids(&self, page_size: u32) -> BlockIdsPage {
        BlockIdsPage::new(self.db.clone(), page_size)
    }

    pub async fn debug_print_root_node(&self, printer: DebugPrinter) {
        match self.acquire_read().await {
            Ok(mut reader) => root_node::debug_print(reader.db(), printer).await,
            Err(error) => printer.display(&format!("Failed to acquire reader {:?}", error)),
        }
    }

    /// Closes the store. Waits until all `Reader`s and `{Read|Write}Transactions` obtained from
    /// this store are dropped.
    pub async fn close(&self) -> Result<(), Error> {
        Ok(self.db.close().await?)
    }

    /// Access the underlying database pool.
    /// TODO: make this non-public when the store extraction is complete.
    pub fn db(&self) -> &db::Pool {
        &self.db
    }
}

/// Read-only operations. This is an up-to-date view of the data.
pub(crate) struct Reader {
    inner: Handle,
    cache: CacheTransaction,
    block_expiration_tracker: Option<Arc<BlockExpirationTracker>>,
}

impl Reader {
    /// Reads a block from the store into a buffer.
    ///
    /// # Panics
    ///
    /// Panics if `buffer` length is less than [`BLOCK_SIZE`].
    pub async fn read_block(
        &mut self,
        id: &BlockId,
        content: &mut BlockContent,
    ) -> Result<BlockNonce, Error> {
        let result = block::read(self.db(), id, content).await;

        if let Some(expiration_tracker) = &self.block_expiration_tracker {
            let is_missing = matches!(result, Err(Error::BlockNotFound));
            expiration_tracker.handle_block_update(id, is_missing);
        }

        result
    }

    /// Checks whether the block exists in the store.
    pub async fn block_exists(&mut self, id: &BlockId) -> Result<bool, Error> {
        block::exists(self.db(), id).await
    }

    /// Returns the total number of blocks in the store.
    pub async fn count_blocks(&mut self) -> Result<u64, Error> {
        block::count(self.db()).await
    }

    /// Returns the number of distinct block ids referenced in the index.
    pub async fn count_block_ids(&mut self) -> Result<u64, Error> {
        leaf_node::count_block_ids(self.db()).await
    }

    #[cfg(test)]
    pub async fn count_leaf_nodes_in_branch(
        &mut self,
        branch_id: &PublicKey,
    ) -> Result<usize, Error> {
        let root_hash = self
            .load_latest_approved_root_node(branch_id, RootNodeFilter::Any)
            .await?
            .proof
            .hash;
        leaf_node::count_in(self.db(), 0, &root_hash).await
    }

    /// Load the latest approved root node of the given branch.
    pub async fn load_latest_approved_root_node(
        &mut self,
        branch_id: &PublicKey,
        filter: RootNodeFilter,
    ) -> Result<RootNode, Error> {
        let node = if let Some(node) = self.cache.get_root(branch_id) {
            node
        } else {
            root_node::load_latest_approved(self.db(), branch_id).await?
        };

        match filter {
            RootNodeFilter::Any => Ok(node),
            RootNodeFilter::Published => {
                let mut new = node;

                while let Some(old) = self.load_prev_approved_root_node(&new).await? {
                    if new.proof.version_vector > old.proof.version_vector {
                        break;
                    } else {
                        new = old;
                    }
                }

                Ok(new)
            }
        }
    }

    pub async fn load_prev_approved_root_node(
        &mut self,
        node: &RootNode,
    ) -> Result<Option<RootNode>, Error> {
        root_node::load_prev_approved(self.db(), node).await
    }

    pub fn load_writer_ids(&mut self) -> impl Stream<Item = Result<PublicKey, Error>> + '_ {
        root_node::load_writer_ids(self.db())
    }

    pub fn load_latest_approved_root_nodes(
        &mut self,
    ) -> impl Stream<Item = Result<RootNode, Error>> + '_ {
        root_node::load_all_latest_approved(self.db())
    }

    pub fn load_latest_preferred_root_nodes(
        &mut self,
    ) -> impl Stream<Item = Result<RootNode, Error>> + '_ {
        root_node::load_all_latest_preferred(self.db())
    }

    #[cfg(test)]
    pub fn load_root_nodes_by_writer<'a>(
        &'a mut self,
        writer_id: &'a PublicKey,
    ) -> impl Stream<Item = Result<RootNode, Error>> + 'a {
        root_node::load_all_by_writer(self.db(), writer_id)
    }

    pub async fn root_node_exists(&mut self, node: &RootNode) -> Result<bool, Error> {
        root_node::exists(self.db(), node).await
    }

    // TODO: use cache and remove `ReadTransaction::load_inner_nodes_with_cache`
    pub async fn load_inner_nodes(&mut self, parent_hash: &Hash) -> Result<InnerNodes, Error> {
        inner_node::load_children(self.db(), parent_hash).await
    }

    // TODO: use cache and remove `ReadTransaction::load_leaf_nodes_with_cache`
    pub async fn load_leaf_nodes(&mut self, parent_hash: &Hash) -> Result<LeafNodes, Error> {
        leaf_node::load_children(self.db(), parent_hash).await
    }

    pub fn load_locators<'a>(
        &'a mut self,
        block_id: &'a BlockId,
    ) -> impl Stream<Item = Result<Hash, Error>> + 'a {
        leaf_node::load_locators(self.db(), block_id)
    }
    // Access the underlying database connection.
    // TODO: Make this private, but first we need to move the `metadata` module to `store`.
    pub(crate) fn db(&mut self) -> &mut db::Connection {
        &mut self.inner
    }
}

/// Read-only transaction. This is a snapshot of the data at the time the transaction was
/// acquired.
pub(crate) struct ReadTransaction {
    inner: Reader,
}

impl ReadTransaction {
    /// Finds the block id corresponding to the given locator in the given branch.
    pub async fn find_block(
        &mut self,
        branch_id: &PublicKey,
        encoded_locator: &Hash,
    ) -> Result<BlockId, Error> {
        let root_node = self
            .load_latest_approved_root_node(branch_id, RootNodeFilter::Any)
            .await?;
        self.find_block_at(&root_node, encoded_locator).await
    }

    pub async fn find_block_at(
        &mut self,
        root_node: &RootNode,
        encoded_locator: &Hash,
    ) -> Result<BlockId, Error> {
        // TODO: On cache miss load only the one node we actually need per layer.

        let mut parent_hash = root_node.proof.hash;

        for layer in 0..INNER_LAYER_COUNT {
            parent_hash = self
                .load_inner_nodes_with_cache(&parent_hash)
                .await?
                .get(get_bucket(encoded_locator, layer))
                .ok_or(Error::LocatorNotFound)?
                .hash;
        }

        self.load_leaf_nodes_with_cache(&parent_hash)
            .await?
            .get(encoded_locator)
            .map(|node| node.block_id)
            .ok_or(Error::LocatorNotFound)
    }

    async fn load_inner_nodes_with_cache(
        &mut self,
        parent_hash: &Hash,
    ) -> Result<InnerNodes, Error> {
        if let Some(nodes) = self.cache.get_inners(parent_hash) {
            return Ok(nodes);
        }

        self.load_inner_nodes(parent_hash).await
    }

    async fn load_leaf_nodes_with_cache(&mut self, parent_hash: &Hash) -> Result<LeafNodes, Error> {
        if let Some(nodes) = self.cache.get_leaves(parent_hash) {
            return Ok(nodes);
        }

        self.load_leaf_nodes(parent_hash).await
    }
}

impl Deref for ReadTransaction {
    type Target = Reader;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for ReadTransaction {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

pub(crate) struct WriteTransaction {
    inner: ReadTransaction,
    untrack_blocks: Option<block_expiration_tracker::UntrackTransaction>,
}

impl WriteTransaction {
    /// Removes the specified block from the store and marks it as missing in the index.
    pub async fn remove_block(&mut self, id: &BlockId) -> Result<(), Error> {
        let (db, cache) = self.db_and_cache();

        block::remove(db, id).await?;
        let parent_hashes = leaf_node::set_missing(db, id).try_collect().await?;
        index::update_summaries(db, cache, parent_hashes).await?;

        let WriteTransaction {
            inner:
                ReadTransaction {
                    inner:
                        Reader {
                            block_expiration_tracker,
                            ..
                        },
                },
            untrack_blocks,
        } = self;

        if let Some(tracker) = block_expiration_tracker {
            let untrack_tx = untrack_blocks.get_or_insert_with(|| tracker.begin_untrack_blocks());
            untrack_tx.untrack(*id);
        }

        Ok(())
    }

    pub async fn remove_branch(&mut self, root_node: &RootNode) -> Result<(), Error> {
        root_node::remove_older(self.db(), root_node).await?;
        root_node::remove(self.db(), root_node).await?;

        self.inner
            .inner
            .cache
            .remove_root(&root_node.proof.writer_id);

        Ok(())
    }

    #[cfg(test)]
    pub async fn clone_root_node_into(
        &mut self,
        src: RootNode,
        dst_writer_id: PublicKey,
        write_keys: &crate::crypto::sign::Keypair,
    ) -> Result<RootNode, Error> {
        use crate::protocol::Proof;

        let hash = src.proof.hash;
        let vv = src.proof.into_version_vector();
        let proof = Proof::new(dst_writer_id, vv, hash, write_keys);

        let (root_node, _) =
            root_node::create(self.db(), proof, src.summary, RootNodeFilter::Any).await?;

        Ok(root_node)
    }

    pub async fn commit(self) -> Result<(), Error> {
        let inner = self.inner.inner.inner.into_write();
        let cache = self.inner.inner.cache;

        match (cache.is_dirty(), self.untrack_blocks) {
            (true, Some(untrack)) => {
                inner
                    .commit_and_then(move || {
                        cache.commit();
                        untrack.commit();
                    })
                    .await?
            }
            (false, Some(untrack)) => {
                inner
                    .commit_and_then(move || {
                        untrack.commit();
                    })
                    .await?
            }
            (true, None) => {
                inner
                    .commit_and_then(move || {
                        cache.commit();
                    })
                    .await?
            }
            (false, None) => {
                inner.commit().await?;
            }
        };

        Ok(())
    }

    /// Commits the transaction and if (and only if) the commit completes successfully, runs the
    /// given closure.
    ///
    /// See `db::WriteTransaction::commit_and_then` for explanation why this is necessary.
    pub async fn commit_and_then<F, R>(self, f: F) -> Result<R, Error>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let inner = self.inner.inner.inner.into_write();
        let cache = self.inner.inner.cache;

        Ok(match (cache.is_dirty(), self.untrack_blocks) {
            (true, Some(untrack)) => {
                inner
                    .commit_and_then(move || {
                        cache.commit();
                        untrack.commit();
                        f()
                    })
                    .await?
            }
            (false, Some(untrack)) => {
                inner
                    .commit_and_then(move || {
                        untrack.commit();
                        f()
                    })
                    .await?
            }
            (true, None) => {
                inner
                    .commit_and_then(move || {
                        cache.commit();
                        f()
                    })
                    .await?
            }
            (false, None) => inner.commit_and_then(f).await?,
        })

        //Ok(inner.commit_and_then(then).await?)
    }

    // Access the underlying database transaction.
    fn db(&mut self) -> &mut db::WriteTransaction {
        self.inner.inner.inner.as_write()
    }

    fn db_and_cache(&mut self) -> (&mut db::WriteTransaction, &mut CacheTransaction) {
        (
            self.inner.inner.inner.as_write(),
            &mut self.inner.inner.cache,
        )
    }
}

impl Deref for WriteTransaction {
    type Target = ReadTransaction;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for WriteTransaction {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

enum Handle {
    Connection(db::PoolConnection),
    ReadTransaction(db::ReadTransaction),
    WriteTransaction(db::WriteTransaction),
}

impl Handle {
    fn as_write(&mut self) -> &mut db::WriteTransaction {
        match self {
            Handle::WriteTransaction(tx) => tx,
            Handle::Connection(_) | Handle::ReadTransaction(_) => unreachable!(),
        }
    }

    fn into_write(self) -> db::WriteTransaction {
        match self {
            Handle::WriteTransaction(tx) => tx,
            Handle::Connection(_) | Handle::ReadTransaction(_) => unreachable!(),
        }
    }
}

impl Deref for Handle {
    type Target = db::Connection;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Connection(conn) => conn,
            Self::ReadTransaction(tx) => tx,
            Self::WriteTransaction(tx) => tx,
        }
    }
}

impl DerefMut for Handle {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            Self::Connection(conn) => conn,
            Self::ReadTransaction(tx) => &mut *tx,
            Self::WriteTransaction(tx) => &mut *tx,
        }
    }
}
