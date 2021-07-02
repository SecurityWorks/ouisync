use crate::{
    directory::Directory,
    entry::EntryType,
    error::{Error, Result},
    replica_id::ReplicaId,
    file::File,
};
use std::collections::BTreeMap;

pub enum JointEntry {
    File(File),
    Directory(BTreeMap<ReplicaId, Directory>),
}

#[allow(clippy::len_without_is_empty)]
impl JointEntry {
    pub fn entry_type(&self) -> EntryType {
        match self {
            Self::File(_) => EntryType::File,
            Self::Directory(_) => EntryType::Directory,
        }
    }

    pub fn as_file(&self) -> Result<&File> {
        match self {
            Self::File(file) => Ok(file),
            Self::Directory(_) => Err(Error::EntryNotDirectory)
        }
    }

    pub fn as_directories(&self) -> Result<&BTreeMap<ReplicaId, Directory>> {
        match self {
            Self::File(_) => Err(Error::EntryIsDirectory),
            Self::Directory(dirs) => Ok(dirs)
        }
    }

    /// Length of the entry in bytes.
    pub fn len(&self) -> u64 {
        match self {
            Self::File(file) => file.len(),
            Self::Directory(dirs) => {
                dirs.values().fold(0, |l, dir| l + dir.len())
            },
        }
    }
}
