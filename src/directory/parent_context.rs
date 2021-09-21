use super::inner::{self, Inner};
use crate::{
    blob_id::BlobId, branch::Branch, db, directory::Directory, error::Result,
    replica_id::ReplicaId, version_vector::VersionVector,
};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Info about an entry in the context of its parent directory.
#[derive(Clone)]
pub(crate) struct ParentContext {
    /// The shared part of the parent directory of the entry.
    directory_inner: Arc<RwLock<Inner>>,
    /// The name of the entry in its parent directory.
    entry_name: String,
    /// Author of the particular version of entry, i.e. the ID of the replica last to have
    /// incremented the version vector.
    entry_author: ReplicaId,
}

impl ParentContext {
    pub(super) fn new(
        directory_inner: Arc<RwLock<Inner>>,
        entry_name: String,
        entry_author: ReplicaId,
    ) -> Self {
        Self {
            directory_inner,
            entry_name,
            entry_author,
        }
    }

    /// Atomically finalizes any pending modifications to the entry that holds this parent context.
    /// This updates the version vector and possibly author id of this entry and all its ancestors,
    /// flushes them and finally commits the transaction.
    ///
    /// If an error occurs anywhere in the process, all intermediate changes are rolled back and all
    /// the affected directories are reverted to their state before calling this function.
    pub async fn modify_entry(
        &mut self,
        tx: db::Transaction<'_>,
        local_id: ReplicaId,
        version_vector_override: Option<&VersionVector>,
    ) -> Result<()> {
        inner::modify_entry(
            tx,
            self.directory_inner.write().await,
            local_id,
            &self.entry_name,
            &mut self.entry_author,
            version_vector_override,
        )
        .await
    }

    /// Forks the parent directory and inserts the entry into it as file, returning its new blob id.
    pub async fn fork_file(&mut self, local_branch: Branch) -> Result<BlobId> {
        let old_vv = self.entry_version_vector().await;

        let outer = self.directory(local_branch);
        let outer = outer.fork().await?;

        let blob_id = outer
            .insert_file_entry(self.entry_name.clone(), self.entry_author, old_vv)
            .await?;

        self.directory_inner = outer.inner;

        Ok(blob_id)
    }

    pub(super) fn entry_name(&self) -> &str {
        &self.entry_name
    }

    /// Returns the parent directory of the entry bound to the given local branch.
    pub(super) fn directory(&self, local_branch: Branch) -> Directory {
        Directory {
            inner: self.directory_inner.clone(),
            local_branch,
        }
    }

    // TODO: Can this be done without cloning the VersionVector? E.g. by returning some kind of
    // read lock.
    pub(super) async fn entry_version_vector(&self) -> VersionVector {
        self.directory_inner
            .read()
            .await
            .entry_version_vector(&self.entry_name, &self.entry_author)
            .cloned()
            .unwrap_or_default()
    }
}