use crate::format;
use sha3::digest::generic_array::typenum::Unsigned;
use sha3::{
    digest::{
        generic_array::{sequence::GenericSequence, GenericArray},
        Digest,
    },
    Sha3_256,
};
use std::{
    array::TryFromSliceError,
    convert::{TryFrom, TryInto},
    fmt,
};

/// Wrapper for a 256-bit hash digest, for convenience. Also implements friendly formatting.
#[derive(Copy, Clone, Eq, PartialEq, Hash)]
#[repr(transparent)]
pub struct Hash(Inner);

impl Hash {
    pub fn null() -> Self {
        Self(Inner::default())
    }
}

impl From<Inner> for Hash {
    fn from(inner: Inner) -> Self {
        Hash(inner)
    }
}

impl AsRef<[u8]> for Hash {
    fn as_ref(&self) -> &[u8] {
        self.0.as_slice()
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:x}", self)
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:8x}", self)
    }
}

impl fmt::LowerHex for Hash {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        format::hex(f, &self.0)
    }
}

impl TryFrom<&'_ [u8]> for Hash {
    type Error = TryFromSliceError;

    fn try_from(slice: &[u8]) -> Result<Self, Self::Error> {
        const USIZE: usize = <Inner as GenericSequence<_>>::Length::USIZE;
        let slice: [u8; USIZE] = slice.try_into()?;
        Ok(Self(slice.into()))
    }
}

type Inner = GenericArray<u8, <Sha3_256 as Digest>::OutputSize>;
