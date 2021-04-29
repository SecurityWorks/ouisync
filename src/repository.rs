use crate::{
    crypto::Cryptor,
    db,
    directory::Directory,
    entry::{Entry, EntryType},
    error::{Error, Result},
    file::File,
    locator::Locator,
};

pub struct Repository {
    pool: db::Pool,
    cryptor: Cryptor,
}

impl Repository {
    pub fn new(pool: db::Pool, cryptor: Cryptor) -> Self {
        Self { pool, cryptor }
    }

    /// Open an entry (file or directory).
    pub async fn open_entry(&self, locator: Locator, entry_type: EntryType) -> Result<Entry> {
        match entry_type {
            EntryType::File => Ok(Entry::File(self.open_file(locator).await?)),
            EntryType::Directory => Ok(Entry::Directory(self.open_directory(locator).await?)),
        }
    }

    pub async fn open_file(&self, _locator: Locator) -> Result<File> {
        todo!()
    }

    pub async fn open_directory(&self, locator: Locator) -> Result<Directory> {
        match Directory::open(self.pool.clone(), self.cryptor.clone(), locator).await {
            Ok(dir) => Ok(dir),
            Err(Error::BlockIdNotFound) if locator == Locator::Root => {
                // Lazily Create the root directory
                Ok(Directory::create(
                    self.pool.clone(),
                    self.cryptor.clone(),
                    Locator::Root,
                ))
            }
            Err(error) => Err(error),
        }
    }
}
