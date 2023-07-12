//! Receiving data from other replicas and writing them into the store.

use super::{
    error::Error,
    quota::{self, QuotaError},
    root_node,
};
use crate::{
    crypto::{sign::PublicKey, Hash},
    db,
    future::try_collect_into,
    index::{update_summaries, NodeState, UpdateSummaryReason},
    storage_size::StorageSize,
};
use sqlx::Row;

/// Status of receiving nodes from remote replica.
#[derive(Debug)]
pub(super) struct ReceiveStatus {
    /// Whether any of the snapshots were already approved.
    pub old_approved: bool,
    /// List of branches whose snapshots have been approved.
    pub new_approved: Vec<PublicKey>,
}

/// Does a parent node (root or inner) with the given hash exist?
pub(super) async fn parent_exists(conn: &mut db::Connection, hash: &Hash) -> Result<bool, Error> {
    Ok(sqlx::query(
        "SELECT
             EXISTS(SELECT 0 FROM snapshot_root_nodes  WHERE hash = ?) OR
             EXISTS(SELECT 0 FROM snapshot_inner_nodes WHERE hash = ?)",
    )
    .bind(hash)
    .bind(hash)
    .fetch_one(conn)
    .await?
    .get(0))
}

pub(super) async fn finalize(
    tx: &mut db::WriteTransaction,
    hash: Hash,
    quota: Option<StorageSize>,
) -> Result<ReceiveStatus, Error> {
    // TODO: Don't hold write transaction through this whole function. Use it only for
    // `update_summaries` then commit it, then do the quota check with a read-only transaction
    // and then grab another write transaction to do the `approve` / `reject`.
    // CAVEAT: the quota check would need some kind of unique lock to prevent multiple
    // concurrent checks to succeed where they would otherwise fail if ran sequentially.

    let states = update_summaries(tx, vec![hash], UpdateSummaryReason::Other).await?;

    let mut old_approved = false;
    let mut new_approved = Vec::new();

    for (hash, state) in states {
        match state {
            NodeState::Complete => (),
            NodeState::Approved => {
                old_approved = true;
                continue;
            }
            NodeState::Incomplete | NodeState::Rejected => continue,
        }

        let approve = if let Some(quota) = quota {
            match quota::check(tx, &hash, quota).await {
                Ok(()) => true,
                Err(QuotaError::Exceeded(size)) => {
                    tracing::warn!(?hash, quota = %quota, size = %size, "snapshot rejected - quota exceeded");
                    false
                }
                Err(QuotaError::Outdated) => {
                    tracing::debug!(?hash, "snapshot outdated");
                    false
                }
                Err(QuotaError::Store(error)) => return Err(error),
            }
        } else {
            true
        };

        if approve {
            root_node::approve(tx, &hash).await?;
            try_collect_into(root_node::load_writer_ids(tx, &hash), &mut new_approved).await?;
        } else {
            root_node::reject(tx, &hash).await?;
        }
    }

    Ok(ReceiveStatus {
        old_approved,
        new_approved,
    })
}

#[cfg(test)]
mod tests {
    use super::super::{
        inner_node::{self, InnerNode, InnerNodeMap, EMPTY_INNER_HASH},
        leaf_node::{self, LeafNode, LeafNodeSet, EMPTY_LEAF_HASH},
        root_node,
    };
    use super::*;
    use crate::{
        crypto::{
            sign::{Keypair, PublicKey},
            Hashable,
        },
        index::{self, node_test_utils::Snapshot, MultiBlockPresence, Proof, Summary},
        test_utils,
        version_vector::VersionVector,
    };
    use assert_matches::assert_matches;
    use rand::{rngs::StdRng, SeedableRng};
    use std::iter;
    use tempfile::TempDir;
    use test_strategy::proptest;

    #[tokio::test(flavor = "multi_thread")]
    async fn compute_summary_from_empty_leaf_nodes() {
        let (_base_dir, pool) = setup().await;
        let mut conn = pool.acquire().await.unwrap();

        let hash = *EMPTY_LEAF_HASH;
        let summary = inner_node::compute_summary(&mut conn, &hash).await.unwrap();

        assert_eq!(summary.state, NodeState::Complete);
        assert_eq!(summary.block_presence, MultiBlockPresence::None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn compute_summary_from_incomplete_leaf_nodes() {
        let (_base_dir, pool) = setup().await;
        let mut conn = pool.acquire().await.unwrap();

        let node = LeafNode::missing(rand::random(), rand::random());
        let nodes: LeafNodeSet = iter::once(node).collect();
        let hash = nodes.hash();

        let summary = inner_node::compute_summary(&mut conn, &hash).await.unwrap();

        assert_eq!(summary, Summary::INCOMPLETE);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn compute_summary_from_complete_leaf_nodes_with_all_missing_blocks() {
        let (_base_dir, pool) = setup().await;

        let node = LeafNode::missing(rand::random(), rand::random());
        let nodes: LeafNodeSet = iter::once(node).collect();
        let hash = nodes.hash();

        let mut tx = pool.begin_write().await.unwrap();
        leaf_node::save_all(&mut tx, &nodes, &hash).await.unwrap();
        let summary = inner_node::compute_summary(&mut tx, &hash).await.unwrap();
        tx.commit().await.unwrap();

        assert_eq!(summary.state, NodeState::Complete);
        assert_eq!(summary.block_presence, MultiBlockPresence::None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn compute_summary_from_complete_leaf_nodes_with_some_present_blocks() {
        let (_base_dir, pool) = setup().await;

        let node0 = LeafNode::present(rand::random(), rand::random());
        let node1 = LeafNode::missing(rand::random(), rand::random());
        let node2 = LeafNode::missing(rand::random(), rand::random());
        let nodes: LeafNodeSet = vec![node0, node1, node2].into_iter().collect();
        let hash = nodes.hash();

        let mut tx = pool.begin_write().await.unwrap();
        leaf_node::save_all(&mut tx, &nodes, &hash).await.unwrap();
        let summary = inner_node::compute_summary(&mut tx, &hash).await.unwrap();
        tx.commit().await.unwrap();

        assert_eq!(summary.state, NodeState::Complete);
        assert_matches!(summary.block_presence, MultiBlockPresence::Some(_));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn compute_summary_from_complete_leaf_nodes_with_all_present_blocks() {
        let (_base_dir, pool) = setup().await;

        let node0 = LeafNode::present(rand::random(), rand::random());
        let node1 = LeafNode::present(rand::random(), rand::random());
        let nodes: LeafNodeSet = vec![node0, node1].into_iter().collect();
        let hash = nodes.hash();

        let mut tx = pool.begin_write().await.unwrap();
        leaf_node::save_all(&mut tx, &nodes, &hash).await.unwrap();
        let summary = inner_node::compute_summary(&mut tx, &hash).await.unwrap();
        tx.commit().await.unwrap();

        assert_eq!(summary.state, NodeState::Complete);
        assert_eq!(summary.block_presence, MultiBlockPresence::Full);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn compute_summary_from_empty_inner_nodes() {
        let (_base_dir, pool) = setup().await;
        let mut conn = pool.acquire().await.unwrap();

        let hash = *EMPTY_INNER_HASH;
        let summary = inner_node::compute_summary(&mut conn, &hash).await.unwrap();

        assert_eq!(summary.state, NodeState::Complete);
        assert_eq!(summary.block_presence, MultiBlockPresence::None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn compute_summary_from_incomplete_inner_nodes() {
        let (_base_dir, pool) = setup().await;
        let mut conn = pool.acquire().await.unwrap();

        let node = InnerNode::new(rand::random(), Summary::INCOMPLETE);
        let nodes: InnerNodeMap = iter::once((0, node)).collect();
        let hash = nodes.hash();

        let summary = inner_node::compute_summary(&mut conn, &hash).await.unwrap();

        assert_eq!(summary, Summary::INCOMPLETE);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn compute_summary_from_complete_inner_nodes_with_all_missing_blocks() {
        let (_base_dir, pool) = setup().await;

        let inners: InnerNodeMap = (0..2)
            .map(|bucket| {
                let leaf = LeafNode::missing(rand::random(), rand::random());
                let leaf_nodes: LeafNodeSet = iter::once(leaf).collect();

                (
                    bucket,
                    InnerNode::new(leaf_nodes.hash(), Summary::from_leaves(&leaf_nodes)),
                )
            })
            .collect();

        let hash = inners.hash();

        let mut tx = pool.begin_write().await.unwrap();
        inner_node::save_all(&mut tx, &inners, &hash).await.unwrap();
        let summary = inner_node::compute_summary(&mut tx, &hash).await.unwrap();
        tx.commit().await.unwrap();

        assert_eq!(summary.state, NodeState::Complete);
        assert_eq!(summary.block_presence, MultiBlockPresence::None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn compute_summary_from_complete_inner_nodes_with_some_present_blocks() {
        let (_base_dir, pool) = setup().await;

        // all missing
        let inner0 = {
            let leaf_nodes: LeafNodeSet = (0..2)
                .map(|_| LeafNode::missing(rand::random(), rand::random()))
                .collect();

            InnerNode::new(leaf_nodes.hash(), Summary::from_leaves(&leaf_nodes))
        };

        // some present
        let inner1 = {
            let leaf_nodes: LeafNodeSet = vec![
                LeafNode::missing(rand::random(), rand::random()),
                LeafNode::present(rand::random(), rand::random()),
            ]
            .into_iter()
            .collect();

            InnerNode::new(leaf_nodes.hash(), Summary::from_leaves(&leaf_nodes))
        };

        // all present
        let inner2 = {
            let leaf_nodes: LeafNodeSet = (0..2)
                .map(|_| LeafNode::present(rand::random(), rand::random()))
                .collect();

            InnerNode::new(leaf_nodes.hash(), Summary::from_leaves(&leaf_nodes))
        };

        let inners: InnerNodeMap = vec![(0, inner0), (1, inner1), (2, inner2)]
            .into_iter()
            .collect();
        let hash = inners.hash();

        let mut tx = pool.begin_write().await.unwrap();
        inner_node::save_all(&mut tx, &inners, &hash).await.unwrap();
        let summary = inner_node::compute_summary(&mut tx, &hash).await.unwrap();
        tx.commit().await.unwrap();

        assert_eq!(summary.state, NodeState::Complete);
        assert_matches!(summary.block_presence, MultiBlockPresence::Some(_));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn compute_summary_from_complete_inner_nodes_with_all_present_blocks() {
        let (_base_dir, pool) = setup().await;

        let inners: InnerNodeMap = (0..2)
            .map(|bucket| {
                let leaf_nodes: LeafNodeSet = (0..2)
                    .map(|_| LeafNode::present(rand::random(), rand::random()))
                    .collect();

                (
                    bucket,
                    InnerNode::new(leaf_nodes.hash(), Summary::from_leaves(&leaf_nodes)),
                )
            })
            .collect();

        let hash = inners.hash();

        let mut tx = pool.begin_write().await.unwrap();
        inner_node::save_all(&mut tx, &inners, &hash).await.unwrap();
        let summary = inner_node::compute_summary(&mut tx, &hash).await.unwrap();
        tx.commit().await.unwrap();

        assert_eq!(summary.state, NodeState::Complete);
        assert_eq!(summary.block_presence, MultiBlockPresence::Full);
    }

    #[proptest]
    fn check_complete(
        #[strategy(0usize..=32)] leaf_count: usize,
        #[strategy(test_utils::rng_seed_strategy())] rng_seed: u64,
    ) {
        test_utils::run(check_complete_case(leaf_count, rng_seed))
    }

    async fn check_complete_case(leaf_count: usize, rng_seed: u64) {
        let mut rng = StdRng::seed_from_u64(rng_seed);

        let (_base_dir, pool) = db::create_temp().await.unwrap();
        let mut tx = pool.begin_write().await.unwrap();

        let writer_id = PublicKey::generate(&mut rng);
        let write_keys = Keypair::generate(&mut rng);
        let snapshot = Snapshot::generate(&mut rng, leaf_count);

        let mut root_node = root_node::create(
            &mut tx,
            Proof::new(
                writer_id,
                VersionVector::first(writer_id),
                *snapshot.root_hash(),
                &write_keys,
            ),
            Summary::INCOMPLETE,
        )
        .await
        .unwrap();

        update_summaries(&mut tx, root_node.proof.hash).await;
        root_node.reload(&mut tx).await.unwrap();
        assert_eq!(root_node.summary.state.is_approved(), leaf_count == 0);

        // TODO: consider randomizing the order the nodes are saved so it's not always
        // breadth-first.

        for layer in snapshot.inner_layers() {
            for (parent_hash, nodes) in layer.inner_maps() {
                inner_node::save_all(&mut tx, &nodes.clone().into_incomplete(), parent_hash)
                    .await
                    .unwrap();

                update_summaries(&mut tx, *parent_hash).await;

                root_node.reload(&mut tx).await.unwrap();
                assert!(!root_node.summary.state.is_approved());
            }
        }

        let mut unsaved_leaves = snapshot.leaf_count();

        for (parent_hash, nodes) in snapshot.leaf_sets() {
            leaf_node::save_all(&mut tx, &nodes.clone().into_missing(), parent_hash)
                .await
                .unwrap();
            unsaved_leaves -= nodes.len();

            update_summaries(&mut tx, *parent_hash).await;
            root_node.reload(&mut tx).await.unwrap();

            if unsaved_leaves > 0 {
                assert!(!root_node.summary.state.is_approved());
            }
        }

        assert!(root_node.summary.state.is_approved());

        // HACK: prevent "too many open files" error.
        drop(tx);
        pool.close().await.unwrap();

        async fn update_summaries(tx: &mut db::WriteTransaction, hash: Hash) {
            for (hash, state) in index::update_summaries(tx, vec![hash], UpdateSummaryReason::Other)
                .await
                .unwrap()
            {
                match state {
                    NodeState::Complete => {
                        root_node::approve(tx, &hash).await.unwrap();
                    }
                    NodeState::Incomplete | NodeState::Approved => (),
                    NodeState::Rejected => unreachable!(),
                }
            }
        }
    }

    #[proptest]
    fn summary(
        #[strategy(0usize..=32)] leaf_count: usize,
        #[strategy(test_utils::rng_seed_strategy())] rng_seed: u64,
    ) {
        test_utils::run(summary_case(leaf_count, rng_seed))
    }

    async fn summary_case(leaf_count: usize, rng_seed: u64) {
        let mut rng = StdRng::seed_from_u64(rng_seed);
        let (_base_dir, pool) = db::create_temp().await.unwrap();
        let mut tx = pool.begin_write().await.unwrap();

        let writer_id = PublicKey::generate(&mut rng);
        let write_keys = Keypair::generate(&mut rng);
        let snapshot = Snapshot::generate(&mut rng, leaf_count);

        // Save the snapshot initially with all nodes missing.
        let mut root_node = root_node::create(
            &mut tx,
            Proof::new(
                writer_id,
                VersionVector::first(writer_id),
                *snapshot.root_hash(),
                &write_keys,
            ),
            Summary::INCOMPLETE,
        )
        .await
        .unwrap();

        if snapshot.leaf_count() == 0 {
            super::update_summaries(
                &mut tx,
                vec![root_node.proof.hash],
                UpdateSummaryReason::Other,
            )
            .await
            .unwrap();
        }

        for layer in snapshot.inner_layers() {
            for (parent_hash, nodes) in layer.inner_maps() {
                inner_node::save_all(&mut tx, &nodes.clone().into_incomplete(), parent_hash)
                    .await
                    .unwrap();
            }
        }

        for (parent_hash, nodes) in snapshot.leaf_sets() {
            leaf_node::save_all(&mut tx, &nodes.clone().into_missing(), parent_hash)
                .await
                .unwrap();
            index::update_summaries(&mut tx, vec![*parent_hash], UpdateSummaryReason::Other)
                .await
                .unwrap();
        }

        // Check that initially all blocks are missing
        root_node.reload(&mut tx).await.unwrap();

        assert_eq!(root_node.summary.block_presence, MultiBlockPresence::None);

        let mut received_blocks = 0;

        for block_id in snapshot.blocks().keys() {
            index::receive_block(&mut tx, block_id).await.unwrap();
            received_blocks += 1;

            root_node.reload(&mut tx).await.unwrap();

            if received_blocks < snapshot.blocks().len() {
                assert_matches!(
                    root_node.summary.block_presence,
                    MultiBlockPresence::Some(_)
                );
            } else {
                assert_eq!(root_node.summary.block_presence, MultiBlockPresence::Full);
            }

            // TODO: check also inner and leaf nodes
        }

        // HACK: prevent "too many open files" error.
        drop(tx);
        pool.close().await.unwrap();
    }

    async fn setup() -> (TempDir, db::Pool) {
        db::create_temp().await.unwrap()
    }
}
