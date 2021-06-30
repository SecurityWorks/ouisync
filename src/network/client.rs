use super::{
    message::{Request, Response},
    message_broker::ClientStream,
};
use crate::{
    block::{self, BlockId},
    crypto::{AuthTag, Hash, Hashable},
    error::Result,
    index::{
        self, Index, InnerNodeMap, LeafNodeSet, MissingBlocksSummary, RootNode, INNER_LAYER_COUNT,
    },
    replica_id::ReplicaId,
    version_vector::VersionVector,
};
use std::cmp::Ordering;

pub struct Client {
    index: Index,
    their_replica_id: ReplicaId,
    stream: ClientStream,
}

impl Client {
    pub fn new(index: Index, their_replica_id: ReplicaId, stream: ClientStream) -> Self {
        Self {
            index,
            their_replica_id,
            stream,
        }
    }

    pub async fn run(&mut self) -> Result<()> {
        while self.pull_snapshot().await? {}
        Ok(())
    }

    async fn pull_snapshot(&mut self) -> Result<bool> {
        // Send version vector that is a combination of the versions of our latest snapshot and
        // their latest complete snapshot that we have. This way they respond only when they have
        // something we don't.
        let mut versions = self.latest_local_versions().await?;

        if let Some(node) =
            RootNode::load_latest_complete(&self.index.pool, &self.their_replica_id).await?
        {
            versions.merge(node.versions);
        }

        self.stream
            .send(Request::RootNode(versions))
            .await
            .unwrap_or(());

        while let Some(response) = self.stream.recv().await {
            // Check competion only if the response affects the index (that is, it is not `Block`)
            // to avoid sending unnecessary duplicate `RootNode` requests.
            let check_complete = !matches!(response, Response::Block { .. });

            self.handle_response(response).await?;

            if check_complete && self.is_complete().await? {
                return Ok(true);
            }
        }

        Ok(false)
    }

    async fn handle_response(&self, response: Response) -> Result<()> {
        match response {
            Response::RootNode {
                versions,
                hash,
                missing_blocks,
            } => self.handle_root_node(versions, hash, missing_blocks).await,
            Response::InnerNodes {
                parent_hash,
                inner_layer,
                nodes,
            } => {
                self.handle_inner_nodes(parent_hash, inner_layer, nodes)
                    .await
            }
            Response::LeafNodes { parent_hash, nodes } => {
                self.handle_leaf_nodes(parent_hash, nodes).await
            }
            Response::Block {
                id,
                content,
                auth_tag,
            } => self.handle_block(id, content, auth_tag).await,
        }
    }

    async fn handle_root_node(
        &self,
        versions: VersionVector,
        hash: Hash,
        missing_blocks: MissingBlocksSummary,
    ) -> Result<()> {
        let this_versions = self.latest_local_versions().await?;
        if versions
            .partial_cmp(&this_versions)
            .map(Ordering::is_le)
            .unwrap_or(false)
        {
            return Ok(());
        }

        let updated = self
            .index
            .has_root_node_new_blocks(&self.their_replica_id, &hash, &missing_blocks)
            .await?;
        let node = RootNode::create(
            &self.index.pool,
            &self.their_replica_id,
            versions,
            hash,
            MissingBlocksSummary::ALL,
        )
        .await?;
        index::detect_complete_snapshots(&self.index.pool, hash, 0).await?;

        if updated {
            self.stream
                .send(Request::InnerNodes {
                    parent_hash: node.hash,
                    inner_layer: 0,
                })
                .await
                .unwrap_or(())
        }

        Ok(())
    }

    async fn handle_inner_nodes(
        &self,
        parent_hash: Hash,
        inner_layer: usize,
        nodes: InnerNodeMap,
    ) -> Result<()> {
        if parent_hash != nodes.hash() {
            log::warn!("inner nodes parent hash mismatch");
            return Ok(());
        }

        let updated: Vec<_> = self
            .index
            .find_inner_nodes_with_new_blocks(&parent_hash, &nodes)
            .await?
            .map(|node| node.hash)
            .collect();

        nodes
            .into_missing()
            .save(&self.index.pool, &parent_hash)
            .await?;
        index::detect_complete_snapshots(&self.index.pool, parent_hash, inner_layer).await?;

        for hash in updated {
            self.stream
                .send(child_request(hash, inner_layer))
                .await
                .unwrap_or(())
        }

        Ok(())
    }

    async fn handle_leaf_nodes(&self, parent_hash: Hash, nodes: LeafNodeSet) -> Result<()> {
        if parent_hash != nodes.hash() {
            log::warn!("leaf nodes parent hash mismatch");
            return Ok(());
        }

        self.pull_missing_blocks(&parent_hash, &nodes).await?;

        nodes
            .into_missing()
            .save(&self.index.pool, &parent_hash)
            .await?;
        index::detect_complete_snapshots(&self.index.pool, parent_hash, INNER_LAYER_COUNT).await?;

        Ok(())
    }

    async fn handle_block(&self, id: BlockId, content: Box<[u8]>, auth_tag: AuthTag) -> Result<()> {
        // TODO: how to validate the block?
        let mut tx = self.index.pool.begin().await?;
        block::write(&mut tx, &id, &content, &auth_tag).await?;
        index::receive_block(&mut tx, &id).await?;
        tx.commit().await?;

        Ok(())
    }

    async fn is_complete(&self) -> Result<bool> {
        Ok(
            RootNode::load_latest(&self.index.pool, &self.their_replica_id)
                .await?
                .map(|node| node.is_complete)
                .unwrap_or(false),
        )
    }

    // Returns the versions of the latest snapshot belonging to the local replica.
    async fn latest_local_versions(&self) -> Result<VersionVector> {
        Ok(
            RootNode::load_latest(&self.index.pool, &self.index.this_replica_id)
                .await?
                .map(|node| node.versions)
                .unwrap_or_default(),
        )
    }

    // Download blocks that are missing by us but present in the remote replica.
    async fn pull_missing_blocks(
        &self,
        parent_hash: &Hash,
        remote_nodes: &LeafNodeSet,
    ) -> Result<()> {
        let updated = self
            .index
            .find_leaf_nodes_with_new_blocks(parent_hash, remote_nodes)
            .await?;
        for node in updated {
            // TODO: avoid multiple clients downloading the same block

            self.stream
                .send(Request::Block(node.block_id))
                .await
                .unwrap_or(());
        }

        Ok(())
    }
}

fn child_request(parent_hash: Hash, inner_layer: usize) -> Request {
    if inner_layer < INNER_LAYER_COUNT - 1 {
        Request::InnerNodes {
            parent_hash,
            inner_layer: inner_layer + 1,
        }
    } else {
        Request::LeafNodes { parent_hash }
    }
}
