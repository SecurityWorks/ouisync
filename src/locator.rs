use crate::{
    block::BlockName,
    crypto::{Cryptor, Hash},
};
use sha3::{Digest, Sha3_256};
use std::slice;

/// A type of block identifier similar to `BlockId` but serving a different purpose. While
/// `BlockId` reflects the block content (it changes when the content change), `Locator` reflects
/// the block "location" within the filesystem. `Locator`'s purpose is to answer the question
/// "what is the n-th block of a given blob?".
/// `Locator` is unique only within a branch while `BlockId` is globally unique.
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum Locator {
    /// Locator of the root block, that is, the head block of the root blob.
    Root,
    /// Locator of the head (first) block of a blob. The `BlockName` is the name of the head block
    /// of the directory which contain the blob and the `u32` is the sequence number (position) of
    /// the blob within that directory.
    Head(BlockName, u32),
    /// Locator of a trunk (other than first) block of a blob. The `BlockName` is the name of the
    /// head block of the blob and the `u32` is the sequence number (position) of the block within
    /// its blob.
    Trunk(BlockName, u32),
}

impl Locator {
    /// Name of the head block of the containing blob. Only trunk blocks have head block.
    pub fn head_name(&self) -> Option<&BlockName> {
        match self {
            Self::Root | Self::Head(..) => None,
            Self::Trunk(name, _) => Some(name),
        }
    }

    /// Block number within the containing blob. The head block's `number` is 0, the next one is 1
    /// and so on.
    pub fn number(&self) -> u32 {
        match self {
            Self::Root | Self::Head(..) => 0,
            Self::Trunk(_, seq) => *seq,
        }
    }

    /// One-way encoding of this `Locator` for the use in the index.
    /// Returns `None` only if `self` is `Root`. This is because root locator doesn't need to be
    /// stored in the index.
    pub fn encode(&self, cryptor: &Cryptor) -> Option<Hash> {
        let (name, seq, flag) = match self {
            Self::Root => return None,
            Self::Head(name, seq) => (name, seq, 0),
            Self::Trunk(name, seq) => (name, seq, 1),
        };

        let mut hasher = Sha3_256::new();

        match cryptor {
            Cryptor::ChaCha20Poly1305(key) => {
                let key_hash = Sha3_256::digest(key.as_array().as_slice());
                hasher.update(key_hash);
            }
            Cryptor::Null => {}
        }

        Some(
            hasher
                .chain(name)
                .chain(seq.to_le_bytes())
                .chain(slice::from_ref(&flag))
                .finalize()
                .into(),
        )
    }
}