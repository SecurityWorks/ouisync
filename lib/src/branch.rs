use crate::{
    access_control::AccessKeys,
    blob::{BlobPin, BlobPinSet},
    blob_id::BlobId,
    block::BlockId,
    crypto::sign::PublicKey,
    db,
    debug::DebugPrinter,
    directory::{Directory, EntryRef, MissingBlockStrategy},
    error::{Error, Result},
    event::Event,
    file::{File, FileCache, OpenLock},
    index::BranchData,
    locator::Locator,
    path,
    version_vector::VersionVector,
};
use camino::{Utf8Component, Utf8Path};
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};
use tokio::sync::broadcast;

#[derive(Clone)]
pub struct Branch {
    pool: db::Pool,
    branch_data: BranchData,
    keys: AccessKeys,
    shared: BranchShared,
}

impl Branch {
    pub(crate) fn new(
        pool: db::Pool,
        branch_data: BranchData,
        keys: AccessKeys,
        shared: BranchShared,
    ) -> Self {
        Self {
            pool,
            branch_data,
            keys,
            shared,
        }
    }

    pub fn id(&self) -> &PublicKey {
        self.branch_data.id()
    }

    pub(crate) fn db(&self) -> &db::Pool {
        &self.pool
    }

    pub(crate) fn data(&self) -> &BranchData {
        &self.branch_data
    }

    pub async fn version_vector(&self) -> Result<VersionVector> {
        let mut conn = self.pool.acquire().await?;
        self.branch_data.load_version_vector(&mut conn).await
    }

    pub(crate) fn keys(&self) -> &AccessKeys {
        &self.keys
    }

    pub(crate) async fn open_root(
        &self,
        missing_block_strategy: MissingBlockStrategy,
    ) -> Result<Directory> {
        Directory::open_root(self.clone(), missing_block_strategy).await
    }

    pub(crate) async fn open_or_create_root(&self) -> Result<Directory> {
        Directory::open_or_create_root(self.clone()).await
    }

    /// Ensures that the directory at the specified path exists including all its ancestors.
    /// Note: non-normalized paths (i.e. containing "..") or Windows-style drive prefixes
    /// (e.g. "C:") are not supported.
    pub(crate) async fn ensure_directory_exists(&self, path: &Utf8Path) -> Result<Directory> {
        let mut curr = self.open_or_create_root().await?;

        for component in path.components() {
            match component {
                Utf8Component::RootDir | Utf8Component::CurDir => (),
                Utf8Component::Normal(name) => {
                    let next = match curr.lookup(name) {
                        Ok(EntryRef::Directory(entry)) => {
                            Some(entry.open(MissingBlockStrategy::Fail).await?)
                        }
                        Ok(EntryRef::File(_)) => return Err(Error::EntryIsFile),
                        Ok(EntryRef::Tombstone(_)) | Err(Error::EntryNotFound) => None,
                        Err(error) => return Err(error),
                    };

                    let next = if let Some(next) = next {
                        next
                    } else {
                        curr.create_directory(name.to_string()).await?
                    };

                    curr = next;
                }
                Utf8Component::Prefix(_) | Utf8Component::ParentDir => {
                    return Err(Error::OperationNotSupported)
                }
            }
        }

        Ok(curr)
    }

    pub(crate) async fn ensure_file_exists(&self, path: &Utf8Path) -> Result<File> {
        let (parent, name) = path::decompose(path).ok_or(Error::EntryIsDirectory)?;
        self.ensure_directory_exists(parent)
            .await?
            .create_file(name.to_string())
            .await
    }

    pub(crate) async fn root_block_id(&self) -> Result<BlockId> {
        let mut tx = self.pool.begin_read().await?;
        let (block_id, _) = self
            .data()
            .get(&mut tx, &Locator::ROOT.encode(self.keys().read()))
            .await?;
        Ok(block_id)
    }

    pub(crate) fn acquire_open_lock(&self, blob_id: BlobId) -> Arc<OpenLock> {
        self.shared.file_cache.acquire(*self.id(), blob_id)
    }

    pub(crate) fn is_file_open(&self, blob_id: &BlobId) -> bool {
        self.shared.file_cache.contains(self.id(), blob_id)
    }

    pub(crate) fn is_any_file_open(&self) -> bool {
        self.shared.file_cache.contains_any(self.id())
    }

    pub(crate) fn pin_blob(&self, blob_id: BlobId) -> BlobPin {
        self.shared.blob_pins.acquire(blob_id)
    }

    pub(crate) fn uncommitted_block_counter(&self) -> &AtomicCounter {
        &self.shared.uncommitted_block_counter
    }

    pub async fn debug_print(&self, print: DebugPrinter) {
        match self.open_root(MissingBlockStrategy::Fail).await {
            Ok(root) => root.debug_print(print).await,
            Err(error) => {
                print.display(&format_args!("failed to open root directory: {:?}", error))
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn reopen(self, keys: AccessKeys) -> Self {
        Self {
            pool: self.pool,
            branch_data: self.branch_data,
            keys,
            shared: self.shared,
        }
    }
}

/// State shared among all branches.
#[derive(Clone)]
pub(crate) struct BranchShared {
    pub file_cache: Arc<FileCache>,
    pub blob_pins: Arc<BlobPinSet>,
    // Number of blocks written without committing the shared transaction.
    pub uncommitted_block_counter: Arc<AtomicCounter>,
}

impl BranchShared {
    pub fn new(event_tx: broadcast::Sender<Event>) -> Self {
        Self {
            file_cache: Arc::new(FileCache::new(event_tx)),
            blob_pins: Arc::new(BlobPinSet::new()),
            uncommitted_block_counter: Arc::new(AtomicCounter::new()),
        }
    }
}

pub(crate) struct AtomicCounter(AtomicU32);

impl AtomicCounter {
    pub fn new() -> Self {
        Self(AtomicU32::new(0))
    }

    // Increments the counter by one and returns the new value.
    pub fn increment(&self) -> u32 {
        self.0
            .fetch_add(1, Ordering::Relaxed)
            .checked_add(1)
            .expect("counter out of range")
    }

    // Resets the counter back to zero.
    pub fn reset(&self) {
        self.0.store(0, Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{access_control::WriteSecrets, db, index::Index, locator::Locator};
    use tempfile::TempDir;
    use tokio::sync::broadcast;

    #[tokio::test(flavor = "multi_thread")]
    async fn ensure_root_directory_exists() {
        let (_base_dir, branch) = setup().await;
        let dir = branch.ensure_directory_exists("/".into()).await.unwrap();
        assert_eq!(dir.locator(), &Locator::ROOT);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ensure_subdirectory_exists() {
        let (_base_dir, branch) = setup().await;

        let mut root = branch.open_or_create_root().await.unwrap();

        branch
            .ensure_directory_exists(Utf8Path::new("/dir"))
            .await
            .unwrap();

        root.refresh().await.unwrap();
        let _ = root.lookup("dir").unwrap();
    }

    async fn setup() -> (TempDir, Branch) {
        let (base_dir, pool) = db::create_temp().await.unwrap();

        let writer_id = PublicKey::random();
        let secrets = WriteSecrets::random();
        let repository_id = secrets.id;
        let (event_tx, _) = broadcast::channel(1);

        let index = Index::new(pool.clone(), repository_id, event_tx.clone());

        let branch = index.get_branch(writer_id);
        let branch = Branch::new(pool, branch, secrets.into(), BranchShared::new(event_tx));

        (base_dir, branch)
    }
}
