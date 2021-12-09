use super::{
    message::{Request, Response},
    message_broker::ServerStream,
};
use crate::{
    block::{self, BlockId, BLOCK_SIZE},
    crypto::Hash,
    error::{Error, Result},
    index::{Index, InnerNode, LeafNode, RootNode},
};
use tokio::{pin, select};

pub(crate) struct Server {
    index: Index,
    stream: ServerStream,
    // "Cookie" number that gets included in the next sent `RootNode` response. The client stores
    // it and sends it back in their next `RootNode` request. This is then used by the server to
    // decide whether the client is up to date (if their cookie is equal (or greater) to ours, they
    // are up to date, otherwise they are not). The server increments this every time there is a
    // change to the local branch.
    cookie: u64,
    // Flag indicating whether the server is waiting for a local change before sending a `RootNode`
    // response to the client. This gets set to true when the client send us a `RootNode` request
    // whose cookie is equal (or greater) than ours which indicates that they are up to date with
    // us.
    waiting: bool,
}

impl Server {
    pub fn new(index: Index, stream: ServerStream) -> Self {
        Self {
            index,
            stream,
            cookie: 1,
            waiting: false,
        }
    }

    pub async fn run(&mut self) -> Result<()> {
        let mut local_branch_subscription = self.index.branches().await.local().subscribe();

        let index_closed = self.index.subscribe().closed();
        pin!(index_closed);

        loop {
            select! {
                request = self.stream.recv() => {
                    let request = if let Some(request) = request {
                        request
                    } else {
                        break;
                    };

                    self.handle_request(request).await?
                }
                _ = local_branch_subscription.changed() => self.handle_local_change().await?,
                _ = &mut index_closed => break,
            }
        }

        Ok(())
    }

    async fn handle_local_change(&mut self) -> Result<()> {
        self.cookie = self.cookie.wrapping_add(1);

        if self.waiting {
            self.waiting = false;
            self.handle_root_node(0).await?;
        }

        Ok(())
    }

    async fn handle_request(&mut self, request: Request) -> Result<()> {
        match request {
            Request::RootNode { cookie } => self.handle_root_node(cookie).await,
            Request::InnerNodes {
                parent_hash,
                inner_layer,
            } => self.handle_inner_nodes(parent_hash, inner_layer).await,
            Request::LeafNodes { parent_hash } => self.handle_leaf_nodes(parent_hash).await,
            Request::Block(id) => self.handle_block(id).await,
        }
    }

    async fn handle_root_node(&mut self, cookie: u64) -> Result<()> {
        log::trace!("cookies: server={}, client={}", self.cookie, cookie);

        // Note: the comparison with zero is there to handle the case when the cookie wraps around.
        if cookie < self.cookie || self.cookie == 0 {
            // TODO: send all branches, not just the local one.
            if let Some(node) =
                RootNode::load_latest(&self.index.pool, &self.index.this_writer_id).await?
            {
                self.stream
                    .send(Response::RootNode {
                        cookie: self.cookie,
                        replica_id: self.index.this_writer_id,
                        versions: node.versions,
                        hash: node.hash,
                        summary: node.summary,
                    })
                    .await;
                return Ok(());
            }
        }

        self.waiting = true;

        Ok(())
    }

    async fn handle_inner_nodes(&self, parent_hash: Hash, inner_layer: usize) -> Result<()> {
        let nodes = InnerNode::load_children(&self.index.pool, &parent_hash).await?;

        self.stream
            .send(Response::InnerNodes {
                parent_hash,
                inner_layer,
                nodes,
            })
            .await;

        Ok(())
    }

    async fn handle_leaf_nodes(&self, parent_hash: Hash) -> Result<()> {
        let nodes = LeafNode::load_children(&self.index.pool, &parent_hash).await?;

        self.stream
            .send(Response::LeafNodes { parent_hash, nodes })
            .await;

        Ok(())
    }

    async fn handle_block(&self, id: BlockId) -> Result<()> {
        let mut content = vec![0; BLOCK_SIZE].into_boxed_slice();
        let auth_tag = match block::read(&self.index.pool, &id, &mut content).await {
            Ok(auth_tag) => auth_tag,
            Err(Error::BlockNotFound(_)) => {
                // This is probably a request to an already deleted orphaned block from an outdated
                // branch. It should be safe to ingore this as the client will request the correct
                // blocks when it becomes up to date to our latest branch.
                log::warn!("requested block {:?} not found", id);
                return Ok(());
            }
            Err(error) => return Err(error),
        };

        self.stream
            .send(Response::Block {
                id,
                content,
                auth_tag,
            })
            .await;

        Ok(())
    }
}
