#[cfg(test)]
mod tests;

mod core;
mod operations;

use self::core::Core;
use self::operations::Operations;
use crate::{
    blob_id::BlobId,
    block::{BlockId, BLOCK_SIZE},
    branch::Branch,
    crypto::{Cryptor, NonceSequence},
    db,
    error::Result,
    locator::Locator,
};
use std::{
    convert::TryInto,
    io::SeekFrom,
    ops::{Deref, DerefMut},
    sync::Arc,
};
use tokio::sync::{Mutex, MutexGuard};
use zeroize::Zeroize;

pub struct Blob {
    core: Arc<Mutex<Core>>,
    locator: Locator,
    branch: Branch,
    current_block: OpenBlock,
}

impl Blob {
    pub(crate) fn new(
        core: Arc<Mutex<Core>>,
        locator: Locator,
        branch: Branch,
        current_block: OpenBlock,
    ) -> Self {
        Self {
            core,
            locator,
            branch,
            current_block,
        }
    }

    /// Opens an existing blob.
    pub async fn open(branch: Branch, locator: Locator) -> Result<Self> {
        Core::open_blob(branch, locator).await
    }

    /// Creates a new blob.
    pub fn create(branch: Branch, locator: Locator) -> Self {
        Core::create_blob(branch, locator)
    }

    pub fn branch(&self) -> &Branch {
        &self.branch
    }

    /// Locator of this blob.
    pub fn locator(&self) -> &Locator {
        &self.locator
    }

    pub fn blob_id(&self) -> &BlobId {
        match &self.locator {
            Locator::Head(blob_id) => blob_id,
            _ => unreachable!(),
        }
    }

    pub async fn len(&self) -> u64 {
        self.core.lock().await.len()
    }

    /// Reads data from this blob into `buffer`, advancing the internal cursor. Returns the
    /// number of bytes actually read which might be less than `buffer.len()` if the portion of the
    /// blob past the internal cursor is smaller than `buffer.len()`.
    pub async fn read(&mut self, buffer: &mut [u8]) -> Result<usize> {
        self.lock().await.ops().read(buffer).await
    }

    /// Read all data from this blob from the current seek position until the end and return then
    /// in a `Vec`.
    pub async fn read_to_end(&mut self) -> Result<Vec<u8>> {
        self.lock().await.ops().read_to_end().await
    }

    /// Writes `buffer` into this blob, advancing the blob's internal cursor.
    pub async fn write(&mut self, buffer: &[u8]) -> Result<()> {
        self.lock().await.ops().write(buffer).await
    }

    /// Seek to an offset in the blob.
    ///
    /// It is allowed to specify offset that is outside of the range of the blob but such offset
    /// will be clamped to be within the range.
    ///
    /// Returns the new seek position from the start of the blob.
    pub async fn seek(&mut self, pos: SeekFrom) -> Result<u64> {
        self.lock().await.ops().seek(pos).await
    }

    /// Truncate the blob to the given length.
    pub async fn truncate(&mut self, len: u64) -> Result<()> {
        self.lock().await.ops().truncate(len).await
    }

    /// Flushes this blob, ensuring that all intermediately buffered contents gets written to the
    /// store.
    pub async fn flush(&mut self) -> Result<bool> {
        self.lock().await.ops().flush().await
    }

    /// Removes this blob.
    pub async fn remove(&mut self) -> Result<()> {
        self.lock().await.ops().remove().await
    }

    /// Creates a shallow copy (only the index nodes are copied, not blocks) of this blob into the
    /// specified destination branch and locator.
    pub async fn fork(&mut self, dst_branch: Branch, dst_head_locator: Locator) -> Result<()> {
        self.lock()
            .await
            .ops()
            .fork(dst_branch, dst_head_locator)
            .await
    }

    pub fn db_pool(&self) -> &db::Pool {
        self.branch.db_pool()
    }

    pub fn cryptor(&self) -> &Cryptor {
        self.branch.cryptor()
    }

    async fn lock(&mut self) -> OperationsLock<'_> {
        let core_guard = self.core.lock().await;
        OperationsLock {
            core_guard,
            current_block: &mut self.current_block,
        }
    }
}

struct OperationsLock<'a> {
    core_guard: MutexGuard<'a, Core>,
    current_block: &'a mut OpenBlock,
}

impl<'a> OperationsLock<'a> {
    fn ops(&mut self) -> Operations {
        self.core_guard.operations(&mut self.current_block)
    }
}

// Data for a block that's been loaded into memory and decrypted.
#[derive(Clone)]
pub(crate) struct OpenBlock {
    // Locator of the block.
    pub locator: Locator,
    // Id of the block.
    pub id: BlockId,
    // Decrypted content of the block wrapped in `Cursor` to track the current seek position.
    pub content: Cursor,
    // Was this block modified since the last time it was loaded from/saved to the store?
    pub dirty: bool,
}

impl OpenBlock {
    pub fn new_head(locator: Locator, nonce_sequence: &NonceSequence) -> Self {
        let mut content = Cursor::new(Buffer::new());
        content.write(&nonce_sequence.prefix()[..]);
        content.write_u64(0); // blob length (initially zero)

        Self {
            locator,
            id: rand::random(),
            content,
            dirty: true,
        }
    }
}

// Buffer for keeping loaded block content and also for in-place encryption and decryption.
#[derive(Clone)]
pub(crate) struct Buffer(Box<[u8]>);

impl Buffer {
    pub fn new() -> Self {
        Self(vec![0; BLOCK_SIZE].into_boxed_slice())
    }
}

// Scramble the buffer on drop to prevent leaving decrypted data in memory past the buffer
// lifetime.
impl Drop for Buffer {
    fn drop(&mut self) {
        self.0.zeroize()
    }
}

impl Deref for Buffer {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for Buffer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

// Wrapper for `Buffer` with an internal position which advances when data is read from or
// written to the buffer.
#[derive(Clone)]
pub(crate) struct Cursor {
    pub buffer: Buffer,
    pub pos: usize,
}

impl Cursor {
    pub fn new(buffer: Buffer) -> Self {
        Self { buffer, pos: 0 }
    }

    // Reads data from the buffer into `dst` and advances the internal position. Returns the
    // number of bytes actual read.
    pub fn read(&mut self, dst: &mut [u8]) -> usize {
        let n = (self.buffer.len() - self.pos).min(dst.len());
        dst[..n].copy_from_slice(&self.buffer[self.pos..self.pos + n]);
        self.pos += n;
        n
    }

    // Read data from the buffer into a fixed-length array.
    //
    // # Panics
    //
    // Panics if the remaining length is less than `N`.
    pub fn read_array<const N: usize>(&mut self) -> [u8; N] {
        let array = self.buffer[self.pos..self.pos + N].try_into().unwrap();
        self.pos += N;
        array
    }

    // Read data from the buffer into a `u64`.
    //
    // # Panics
    //
    // Panics if the remaining length is less than `size_of::<u64>()`
    pub fn read_u64(&mut self) -> u64 {
        u64::from_le_bytes(self.read_array())
    }

    // Writes data from `dst` into the buffer and advances the internal position. Returns the
    // number of bytes actually written.
    pub fn write(&mut self, src: &[u8]) -> usize {
        let n = (self.buffer.len() - self.pos).min(src.len());
        self.buffer[self.pos..self.pos + n].copy_from_slice(&src[..n]);
        self.pos += n;
        n
    }

    // Write a `u64` into the buffer.
    //
    // # Panics
    //
    // Panics if the remaining length is less than `size_of::<u64>()`
    pub fn write_u64(&mut self, value: u64) {
        let bytes = value.to_le_bytes();
        assert!(self.buffer.len() - self.pos >= bytes.len());
        self.write(&bytes[..]);
    }
}

impl Deref for Cursor {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.buffer[self.pos..]
    }
}

impl DerefMut for Cursor {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.buffer[self.pos..]
    }
}
