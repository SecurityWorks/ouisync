use std::borrow::Cow;

use super::{
    node::{self, InnerNode, LeafNode, RootNode, SingleBlockPresence, INNER_LAYER_COUNT},
    path::Path,
    proof::Proof,
    Summary, VersionVectorOp,
};
use crate::{
    block::BlockId,
    crypto::{
        sign::{Keypair, PublicKey},
        Hash,
    },
    db,
    error::{Error, Result},
    version_vector::VersionVector,
    versioned::{BranchItem, Versioned},
};
use futures_util::{Stream, TryStreamExt};

type LocatorHash = Hash;

#[derive(Clone)]
pub(crate) struct BranchData {
    writer_id: PublicKey,
}

impl BranchData {
    /// Construct a branch data using the provided root node.
    pub fn new(writer_id: PublicKey) -> Self {
        Self { writer_id }
    }

    /// Load all branches
    pub fn load_all(conn: &mut db::Connection) -> impl Stream<Item = Result<Self>> + '_ {
        SnapshotData::load_all(conn).map_ok(move |snapshot| snapshot.to_branch_data())
    }

    pub async fn load_snapshot(&self, conn: &mut db::Connection) -> Result<SnapshotData> {
        let root_node = RootNode::load_latest_complete_by_writer(conn, self.writer_id).await?;

        Ok(SnapshotData { root_node })
    }

    pub async fn load_or_create_snapshot(
        &self,
        conn: &mut db::Connection,
        write_keys: &Keypair,
    ) -> Result<SnapshotData> {
        let root_node = match RootNode::load_latest_complete_by_writer(conn, self.writer_id).await {
            Ok(root_node) => Some(root_node),
            Err(Error::EntryNotFound) => None,
            Err(error) => return Err(error),
        };

        let root_node = if let Some(root_node) = root_node {
            root_node
        } else {
            RootNode::empty(self.writer_id, write_keys)
        };

        Ok(SnapshotData { root_node })
    }

    /// Returns the id of the replica that owns this branch.
    pub fn id(&self) -> &PublicKey {
        &self.writer_id
    }

    /// Loads the version vector of this branch.
    pub async fn load_version_vector(&self, conn: &mut db::Connection) -> Result<VersionVector> {
        match self.load_snapshot(conn).await {
            Ok(snapshot) => Ok(snapshot.root_node.proof.into_version_vector()),
            Err(Error::EntryNotFound) => Ok(VersionVector::new()),
            Err(error) => Err(error),
        }
    }

    /// Retrieve `BlockId` of a block with the given encoded `Locator`.
    pub async fn get(
        &self,
        tx: &mut db::ReadTransaction,
        encoded_locator: &Hash,
    ) -> Result<(BlockId, SingleBlockPresence)> {
        self.load_snapshot(tx)
            .await?
            .get_block(tx, encoded_locator)
            .await
    }

    #[cfg(test)]
    pub async fn count_leaf_nodes(&self, conn: &mut db::Connection) -> Result<usize> {
        let root_hash = self.load_snapshot(conn).await?.root_node.proof.hash;
        count_leaf_nodes(conn, 0, &root_hash).await
    }
}

pub(crate) struct SnapshotData {
    pub(super) root_node: RootNode,
}

impl SnapshotData {
    /// Load all latest snapshots
    pub fn load_all(conn: &mut db::Connection) -> impl Stream<Item = Result<Self>> + '_ {
        RootNode::load_all_latest_complete(conn).map_ok(move |root_node| Self { root_node })
    }

    /// Load previous snapshot
    pub async fn load_prev(&self, conn: &mut db::Connection) -> Result<Option<Self>> {
        Ok(self
            .root_node
            .load_prev(conn)
            .await?
            .map(|root_node| Self { root_node }))
    }

    /// Returns the id of the replica that owns this branch.
    pub fn branch_id(&self) -> &PublicKey {
        &self.root_node.proof.writer_id
    }

    /// Returns the root node hash of this snapshot.
    pub fn root_hash(&self) -> &Hash {
        &self.root_node.proof.hash
    }

    pub fn to_branch_data(&self) -> BranchData {
        BranchData {
            writer_id: self.root_node.proof.writer_id,
        }
    }

    /// Gets the version vector of this snapshot.
    pub fn version_vector(&self) -> &VersionVector {
        &self.root_node.proof.version_vector
    }

    /// Does this snapshot exist in the db?
    pub async fn exists(&self, conn: &mut db::Connection) -> Result<bool> {
        self.root_node.exists(conn).await
    }

    /// Inserts a new block into the index.
    ///
    /// # Cancel safety
    ///
    /// This operation is executed inside a db transaction which makes it atomic even in the
    /// presence of cancellation.
    pub async fn insert_block(
        &mut self,
        tx: &mut db::WriteTransaction,
        encoded_locator: &LocatorHash,
        block_id: &BlockId,
        block_presence: SingleBlockPresence,
        write_keys: &Keypair,
    ) -> Result<bool> {
        let mut path = self.load_path(tx, encoded_locator).await?;

        if path.has_leaf(block_id) {
            return Ok(false);
        }

        path.set_leaf(block_id, block_presence);

        self.save_path(tx, &path, write_keys).await?;

        Ok(true)
    }

    /// Removes the block identified by `encoded_locator`. If `expected_block_id` is `Some`, then
    /// the block is removed only if its id matches it, otherwise it's removed unconditionally.
    pub async fn remove_block(
        &mut self,
        tx: &mut db::WriteTransaction,
        encoded_locator: &Hash,
        expected_block_id: Option<&BlockId>,
        write_keys: &Keypair,
    ) -> Result<()> {
        let mut path = self.load_path(tx, encoded_locator).await?;

        let block_id = path
            .remove_leaf(encoded_locator)
            .ok_or(Error::EntryNotFound)?;

        if let Some(expected_block_id) = expected_block_id {
            if &block_id != expected_block_id {
                return Ok(());
            }
        }

        self.save_path(tx, &path, write_keys).await?;

        Ok(())
    }

    /// Retrieve `BlockId` of a block with the given encoded `Locator`.
    pub async fn get_block(
        &self,
        tx: &mut db::ReadTransaction,
        encoded_locator: &Hash,
    ) -> Result<(BlockId, SingleBlockPresence)> {
        let path = self.load_path(tx, encoded_locator).await?;
        path.get_leaf().ok_or(Error::EntryNotFound)
    }

    /// Update the root version vector of this branch.
    pub async fn bump(
        &mut self,
        tx: &mut db::WriteTransaction,
        op: VersionVectorOp<'_>,
        write_keys: &Keypair,
    ) -> Result<()> {
        let mut new_vv = self.root_node.proof.version_vector.clone();
        op.apply(self.branch_id(), &mut new_vv);

        // Sometimes `op` is a no-op. This is not an error.
        if new_vv == self.root_node.proof.version_vector {
            return Ok(());
        }

        let new_proof = Proof::new(
            self.root_node.proof.writer_id,
            new_vv,
            self.root_node.proof.hash,
            write_keys,
        );

        self.create_root_node(tx, new_proof, self.root_node.summary)
            .instrument(tracing::info_span!("bump"))
            .await
    }

    /// Remove this snapshot
    pub async fn remove(&self, tx: &mut db::WriteTransaction) -> Result<()> {
        self.root_node.remove_recursively(tx).await
    }

    /// Remove all snapshots of this branch older than this one.
    pub async fn remove_all_older(&self, tx: &mut db::WriteTransaction) -> Result<()> {
        self.root_node.remove_recursively_all_older(tx).await
    }

    /// Prune outdated older snapshots. Note this is not the same as `remove_all_older` because this
    /// preserves older snapshots that can be used as fallback for the latest snapshot and only
    /// removes those that can't.
    pub async fn prune(&self, db: &db::Pool) -> Result<()> {
        // First remove all incomplete snapshots as they can never serve as fallback.
        let mut tx = db.begin_write().await?;
        self.root_node
            .remove_recursively_all_older_incomplete(&mut tx)
            .await?;
        tx.commit().await?;

        let mut conn = db.acquire().await?;

        // Then remove those snapshots that can't serve as fallback for the current one.
        let mut new = Cow::Borrowed(&self.root_node);

        while let Some(old) = new.load_prev(&mut conn).await? {
            if node::check_fallback(&mut conn, &old, &new).await? {
                // `old` can serve as fallback for `self` and so we can't prune it yet. Try the
                // previous snapshot.
                tracing::trace!(
                    branch_id = ?old.proof.writer_id,
                    hash = ?old.proof.hash,
                    vv = ?old.proof.version_vector,
                    "outdated snapshot not removed - possible fallback"
                );

                new = Cow::Owned(old);
            } else {
                // `old` can't serve as fallback for `self` and so we can safely remove it
                let mut tx = db.begin_write().await?;
                old.remove_recursively(&mut tx).await?;
                tx.commit().await?;

                tracing::trace!(
                    branch_id = ?old.proof.writer_id,
                    hash = ?old.proof.hash,
                    vv = ?old.proof.version_vector,
                    "outdated snapshot removed"
                );
            }
        }

        Ok(())
    }

    async fn load_path(
        &self,
        tx: &mut db::ReadTransaction,
        encoded_locator: &LocatorHash,
    ) -> Result<Path> {
        let mut path = Path::new(
            self.root_node.proof.hash,
            self.root_node.summary,
            *encoded_locator,
        );
        let mut parent = path.root_hash;

        for level in 0..INNER_LAYER_COUNT {
            path.inner[level] = InnerNode::load_children(tx, &parent).await?;

            if let Some(node) = path.inner[level].get(path.get_bucket(level)) {
                parent = node.hash
            } else {
                return Ok(path);
            };
        }

        path.leaves = LeafNode::load_children(tx, &parent).await?;

        Ok(path)
    }

    async fn save_path(
        &mut self,
        tx: &mut db::WriteTransaction,
        path: &Path,
        write_keys: &Keypair,
    ) -> Result<()> {
        for (i, inner_layer) in path.inner.iter().enumerate() {
            if let Some(parent_hash) = path.hash_at_layer(i) {
                inner_layer.save(tx, &parent_hash).await?;
            }
        }

        let layer = Path::total_layer_count() - 1;
        if let Some(parent_hash) = path.hash_at_layer(layer - 1) {
            path.leaves.save(tx, &parent_hash).await?;
        }

        let writer_id = self.root_node.proof.writer_id;
        let new_version_vector = self
            .root_node
            .proof
            .version_vector
            .clone()
            .incremented(writer_id);
        let new_proof = Proof::new(writer_id, new_version_vector, path.root_hash, write_keys);

        self.create_root_node(tx, new_proof, path.root_summary)
            .await
    }

    async fn create_root_node(
        &mut self,
        tx: &mut db::WriteTransaction,
        new_proof: Proof,
        new_summary: Summary,
    ) -> Result<()> {
        self.root_node = RootNode::create(tx, new_proof, new_summary).await?;
        self.remove_all_older(tx).await?;

        tracing::trace!(
            vv = ?self.root_node.proof.version_vector,
            hash = ?self.root_node.proof.hash,
            branch_id = ?self.root_node.proof.writer_id,
            "create local snapshot"
        );

        Ok(())
    }
}

impl Versioned for SnapshotData {
    fn version_vector(&self) -> &VersionVector {
        SnapshotData::version_vector(self)
    }
}

impl BranchItem for SnapshotData {
    fn branch_id(&self) -> &PublicKey {
        SnapshotData::branch_id(self)
    }
}

#[cfg(test)]
use async_recursion::async_recursion;
use tracing::Instrument;

#[async_recursion]
#[cfg(test)]
async fn count_leaf_nodes(
    conn: &mut db::Connection,
    current_layer: usize,
    node: &Hash,
) -> Result<usize> {
    if current_layer < INNER_LAYER_COUNT {
        let children = InnerNode::load_children(conn, node).await?;

        let mut sum = 0;

        for (_bucket, child) in children {
            sum += count_leaf_nodes(conn, current_layer + 1, &child.hash).await?;
        }

        Ok(sum)
    } else {
        Ok(LeafNode::load_children(conn, node).await?.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        crypto::{cipher::SecretKey, sign::Keypair},
        index::EMPTY_INNER_HASH,
        locator::Locator,
        state_monitor::StateMonitor,
        test_utils,
    };
    use proptest::{arbitrary::any, collection::vec};
    use rand::{
        rngs::StdRng,
        seq::{IteratorRandom, SliceRandom},
        Rng, SeedableRng,
    };
    use sqlx::Row;
    use std::collections::BTreeMap;
    use tempfile::TempDir;
    use test_strategy::{proptest, Arbitrary};

    #[tokio::test(flavor = "multi_thread")]
    async fn insert_and_read() {
        let (_base_dir, pool, branch) = setup().await;
        let read_key = SecretKey::random();
        let write_keys = Keypair::random();

        let block_id = rand::random();
        let locator = random_head_locator();
        let encoded_locator = locator.encode(&read_key);

        let mut tx = pool.begin_write().await.unwrap();

        branch
            .load_or_create_snapshot(&mut tx, &write_keys)
            .await
            .unwrap()
            .insert_block(
                &mut tx,
                &encoded_locator,
                &block_id,
                SingleBlockPresence::Present,
                &write_keys,
            )
            .await
            .unwrap();

        let (r, _) = branch.get(&mut tx, &encoded_locator).await.unwrap();

        assert_eq!(r, block_id);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rewrite_locator() {
        for _ in 0..32 {
            let (_base_dir, pool, branch) = setup().await;
            let read_key = SecretKey::random();
            let write_keys = Keypair::random();

            let b1 = rand::random();
            let b2 = rand::random();

            let locator = random_head_locator();
            let encoded_locator = locator.encode(&read_key);

            let mut tx = pool.begin_write().await.unwrap();

            branch
                .load_or_create_snapshot(&mut tx, &write_keys)
                .await
                .unwrap()
                .insert_block(
                    &mut tx,
                    &encoded_locator,
                    &b1,
                    SingleBlockPresence::Present,
                    &write_keys,
                )
                .await
                .unwrap();

            branch
                .load_or_create_snapshot(&mut tx, &write_keys)
                .await
                .unwrap()
                .insert_block(
                    &mut tx,
                    &encoded_locator,
                    &b2,
                    SingleBlockPresence::Present,
                    &write_keys,
                )
                .await
                .unwrap();

            let (r, _) = branch.get(&mut tx, &encoded_locator).await.unwrap();
            assert_eq!(r, b2);

            branch
                .load_snapshot(&mut tx)
                .await
                .unwrap()
                .remove_all_older(&mut tx)
                .await
                .unwrap();

            assert_eq!(
                INNER_LAYER_COUNT + 1,
                count_branch_forest_entries(&mut tx).await
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remove_locator() {
        let (_base_dir, pool, branch) = setup().await;
        let read_key = SecretKey::random();
        let write_keys = Keypair::random();

        let b = rand::random();
        let locator = random_head_locator();
        let encoded_locator = locator.encode(&read_key);

        let mut tx = pool.begin_write().await.unwrap();

        assert_eq!(0, count_branch_forest_entries(&mut tx).await);

        branch
            .load_or_create_snapshot(&mut tx, &write_keys)
            .await
            .unwrap()
            .insert_block(
                &mut tx,
                &encoded_locator,
                &b,
                SingleBlockPresence::Present,
                &write_keys,
            )
            .await
            .unwrap();
        let (r, _) = branch.get(&mut tx, &encoded_locator).await.unwrap();
        assert_eq!(r, b);

        assert_eq!(
            INNER_LAYER_COUNT + 1,
            count_branch_forest_entries(&mut tx).await
        );

        branch
            .load_or_create_snapshot(&mut tx, &write_keys)
            .await
            .unwrap()
            .remove_block(&mut tx, &encoded_locator, None, &write_keys)
            .await
            .unwrap();

        match branch.get(&mut tx, &encoded_locator).await {
            Err(Error::EntryNotFound) => { /* OK */ }
            Err(_) => panic!("Error should have been EntryNotFound"),
            Ok(_) => panic!("BranchData shouldn't have contained the block ID"),
        }

        branch
            .load_snapshot(&mut tx)
            .await
            .unwrap()
            .remove_all_older(&mut tx)
            .await
            .unwrap();

        assert_eq!(0, count_branch_forest_entries(&mut tx).await);
    }

    #[proptest]
    fn empty_nodes_are_not_stored(
        #[strategy(1usize..32)] leaf_count: usize,
        #[strategy(test_utils::rng_seed_strategy())] rng_seed: u64,
    ) {
        test_utils::run(empty_nodes_are_not_stored_case(leaf_count, rng_seed))
    }

    async fn empty_nodes_are_not_stored_case(leaf_count: usize, rng_seed: u64) {
        let mut rng = StdRng::seed_from_u64(rng_seed);
        let (_base_dir, pool, branch) = setup().await;
        let write_keys = Keypair::generate(&mut rng);

        let mut locators = Vec::new();
        let mut tx = pool.begin_write().await.unwrap();

        let mut snapshot = branch
            .load_or_create_snapshot(&mut tx, &write_keys)
            .await
            .unwrap();

        // Add blocks
        for _ in 0..leaf_count {
            let locator = rng.gen();
            let block_id = rng.gen();

            snapshot
                .insert_block(
                    &mut tx,
                    &locator,
                    &block_id,
                    SingleBlockPresence::Present,
                    &write_keys,
                )
                .await
                .unwrap();

            locators.push(locator);

            assert!(!has_empty_inner_node(&mut tx).await);
        }

        snapshot.remove_all_older(&mut tx).await.unwrap();

        // Remove blocks
        locators.shuffle(&mut rng);

        for locator in locators {
            snapshot
                .remove_block(&mut tx, &locator, None, &write_keys)
                .await
                .unwrap();
            snapshot.remove_all_older(&mut tx).await.unwrap();

            assert!(!has_empty_inner_node(&mut tx).await);
        }
    }

    #[proptest]
    fn prune(
        #[strategy(vec(any::<PruneTestOp>(), 1..32))] ops: Vec<PruneTestOp>,
        #[strategy(test_utils::rng_seed_strategy())] rng_seed: u64,
    ) {
        test_utils::run(prune_case(ops, rng_seed))
    }

    #[derive(Arbitrary, Debug)]
    enum PruneTestOp {
        Insert,
        Remove,
        Bump,
        Prune,
    }

    async fn prune_case(ops: Vec<PruneTestOp>, rng_seed: u64) {
        let mut rng = StdRng::seed_from_u64(rng_seed);
        let (_base_dir, pool, branch) = setup().await;
        let write_keys = Keypair::generate(&mut rng);

        let mut snapshot = {
            let mut conn = pool.acquire().await.unwrap();
            branch
                .load_or_create_snapshot(&mut conn, &write_keys)
                .await
                .unwrap()
        };

        let mut expected = BTreeMap::new();

        for op in ops {
            // Apply op
            match op {
                PruneTestOp::Insert => {
                    let locator = rng.gen();
                    let block_id = rng.gen();

                    let mut tx = pool.begin_write().await.unwrap();
                    snapshot
                        .insert_block(
                            &mut tx,
                            &locator,
                            &block_id,
                            SingleBlockPresence::Present,
                            &write_keys,
                        )
                        .await
                        .unwrap();
                    tx.commit().await.unwrap();

                    expected.insert(locator, block_id);
                }
                PruneTestOp::Remove => {
                    let Some(locator) = expected.keys().choose(&mut rng).copied() else {
                        continue;
                    };

                    let mut tx = pool.begin_write().await.unwrap();
                    snapshot
                        .remove_block(&mut tx, &locator, None, &write_keys)
                        .await
                        .unwrap();
                    tx.commit().await.unwrap();

                    expected.remove(&locator);
                }
                PruneTestOp::Bump => {
                    let mut tx = pool.begin_write().await.unwrap();
                    snapshot
                        .bump(&mut tx, VersionVectorOp::IncrementLocal, &write_keys)
                        .await
                        .unwrap();
                    tx.commit().await.unwrap();
                }
                PruneTestOp::Prune => {
                    snapshot.prune(&pool).await.unwrap();
                }
            }

            // Verify all expected blocks still present
            let mut tx = pool.begin_read().await.unwrap();

            for (locator, expected_block_id) in &expected {
                let (actual_block_id, _) = snapshot.get_block(&mut tx, locator).await.unwrap();
                assert_eq!(actual_block_id, *expected_block_id);
            }

            // Verify the snapshot is still complete
            check_complete(&mut tx, &snapshot).await;
        }
    }

    #[ignore]
    #[tokio::test(flavor = "multi_thread")]
    async fn fallback() {
        test_utils::init_log();
        let mut rng = StdRng::seed_from_u64(0);
        let (_base_dir, pool, branch0) = setup().await;
        let write_keys = Keypair::generate(&mut rng);

        let mut snapshot = {
            let mut conn = pool.acquire().await.unwrap();
            branch0
                .load_or_create_snapshot(&mut conn, &write_keys)
                .await
                .unwrap()
        };

        let locator = rng.gen();
        let id0 = rng.gen();
        let id1 = rng.gen();
        let id2 = rng.gen();
        let id3 = rng.gen();

        for (block_id, presence) in [
            (id0, SingleBlockPresence::Present),
            (id1, SingleBlockPresence::Present),
            (id2, SingleBlockPresence::Missing),
            (id3, SingleBlockPresence::Missing),
        ] {
            let mut tx = pool.begin_write().await.unwrap();
            snapshot
                .insert_block(&mut tx, &locator, &block_id, presence, &write_keys)
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }

        snapshot.prune(&pool).await.unwrap();

        let mut tx = pool.begin_read().await.unwrap();

        assert_eq!(
            snapshot.get_block(&mut tx, &locator).await.unwrap(),
            (id3, SingleBlockPresence::Missing)
        );

        // The previous snapshot was pruned because it can't serve as fallback for the latest one
        // but the one before it was not because it can.
        let snapshot = snapshot.load_prev(&mut tx).await.unwrap().unwrap();
        assert_eq!(
            snapshot.get_block(&mut tx, &locator).await.unwrap(),
            (id1, SingleBlockPresence::Present)
        );

        // All the further snapshots were pruned as well
        assert!(snapshot.load_prev(&mut tx).await.unwrap().is_none());
    }

    async fn count_branch_forest_entries(conn: &mut db::Connection) -> usize {
        sqlx::query(
            "SELECT
                 (SELECT COUNT(*) FROM snapshot_inner_nodes) +
                 (SELECT COUNT(*) FROM snapshot_leaf_nodes)",
        )
        .fetch_one(conn)
        .await
        .unwrap()
        .get::<u32, _>(0) as usize
    }

    async fn has_empty_inner_node(conn: &mut db::Connection) -> bool {
        sqlx::query("SELECT 0 FROM snapshot_inner_nodes WHERE hash = ? LIMIT 1")
            .bind(&*EMPTY_INNER_HASH)
            .fetch_optional(conn)
            .await
            .unwrap()
            .is_some()
    }

    async fn check_complete(conn: &mut db::Connection, snapshot: &SnapshotData) {
        if snapshot.root_node.proof.hash == *EMPTY_INNER_HASH {
            return;
        }

        let nodes = InnerNode::load_children(conn, &snapshot.root_node.proof.hash)
            .await
            .unwrap();
        assert!(!nodes.is_empty());

        let mut stack: Vec<_> = nodes.into_iter().map(|(_, node)| node).collect();

        while let Some(node) = stack.pop() {
            let inners = InnerNode::load_children(conn, &node.hash).await.unwrap();
            let leaves = LeafNode::load_children(conn, &node.hash).await.unwrap();

            assert!(inners.len() + leaves.len() > 0);

            stack.extend(inners.into_iter().map(|(_, node)| node));
        }
    }

    async fn init_db() -> (TempDir, db::Pool) {
        db::create_temp(&StateMonitor::make_root()).await.unwrap()
    }

    async fn setup() -> (TempDir, db::Pool, BranchData) {
        let (base_dir, pool) = init_db().await;
        let branch = BranchData::new(PublicKey::random());

        (base_dir, pool, branch)
    }

    fn random_head_locator() -> Locator {
        Locator::head(rand::random())
    }
}
