use crate::{
    error::Error,
    registry::{Handle, InvalidHandle, Registry},
    state::{State, TaskHandle},
};
use camino::Utf8PathBuf;
use ouisync_bridge::{protocol::Notification, repository, transport::NotificationSender};
use ouisync_lib::{
    network::{self, Registration},
    path, AccessMode, Event, Payload, Progress, Repository, ShareToken,
};
use std::{
    collections::{hash_map::Entry, HashMap},
    mem,
    path::PathBuf,
    sync::{Arc, RwLock},
};
use tokio::sync::{broadcast::error::RecvError, Notify};

pub(crate) struct RepositoryHolder {
    pub store_path: PathBuf,
    pub repository: Arc<Repository>,
    pub registration: Registration,
}

pub(crate) type RepositoryHandle = Handle<Arc<RepositoryHolder>>;

pub(crate) async fn create(
    state: &State,
    store_path: PathBuf,
    local_read_password: Option<String>,
    local_write_password: Option<String>,
    share_token: Option<ShareToken>,
) -> Result<RepositoryHandle, Error> {
    let entry = ensure_vacant_entry(state, store_path.clone()).await?;

    let repository = repository::create(
        store_path.clone(),
        local_read_password,
        local_write_password,
        share_token,
        &state.config,
        &state.repos_monitor,
    )
    .await?;

    let registration = state.network.register(repository.handle()).await;
    let holder = RepositoryHolder {
        store_path,
        repository: Arc::new(repository),
        registration,
    };

    state
        .mounter
        .mount(&holder.store_path, &holder.repository)?;

    let handle = entry.insert(holder);

    Ok(handle)
}

/// Opens an existing repository.
pub(crate) async fn open(
    state: &State,
    store_path: PathBuf,
    local_password: Option<String>,
) -> Result<RepositoryHandle, Error> {
    let entry = match state.repositories.entry(store_path.clone()).await {
        RepositoryEntry::Occupied(handle) => return Ok(handle),
        RepositoryEntry::Vacant(entry) => entry,
    };

    let repository = repository::open(
        store_path.clone(),
        local_password,
        &state.config,
        &state.repos_monitor,
    )
    .await?;

    let registration = state.network.register(repository.handle()).await;
    let holder = RepositoryHolder {
        store_path,
        repository: Arc::new(repository),
        registration,
    };

    state
        .mounter
        .mount(&holder.store_path, &holder.repository)?;

    let handle = entry.insert(holder);

    Ok(handle)
}

pub(crate) fn create_reopen_token(
    state: &State,
    handle: RepositoryHandle,
) -> Result<Vec<u8>, Error> {
    Ok(state
        .repositories
        .get(handle)?
        .repository
        .reopen_token()
        .encode())
}

pub(crate) async fn reopen(
    state: &State,
    store_path: PathBuf,
    token: Vec<u8>,
) -> Result<RepositoryHandle, Error> {
    let entry = ensure_vacant_entry(state, store_path.clone()).await?;

    let repository = repository::reopen(store_path.clone(), token, &state.repos_monitor).await?;
    let registration = state.network.register(repository.handle()).await;
    let holder = RepositoryHolder {
        store_path,
        repository: Arc::new(repository),
        registration,
    };

    state
        .mounter
        .mount(&holder.store_path, &holder.repository)?;

    let handle = entry.insert(holder);

    Ok(handle)
}

async fn ensure_vacant_entry(
    state: &State,
    store_path: PathBuf,
) -> Result<RepositoryVacantEntry<'_>, ouisync_lib::Error> {
    loop {
        match state.repositories.entry(store_path.clone()).await {
            RepositoryEntry::Occupied(handle) => {
                if let Some(holder) = state.repositories.remove(handle) {
                    holder.repository.close().await?;
                }
            }
            RepositoryEntry::Vacant(entry) => return Ok(entry),
        }
    }
}

/// Closes a repository.
pub(crate) async fn close(state: &State, handle: RepositoryHandle) -> Result<(), Error> {
    if let Some(holder) = state.repositories.remove(handle) {
        holder.repository.close().await?;
        state.mounter.unmount(&holder.store_path)?;
    }

    Ok(())
}

pub(crate) async fn set_read_access(
    state: &State,
    handle: RepositoryHandle,
    local_read_password: Option<String>,
    share_token: Option<ShareToken>,
) -> Result<(), Error> {
    let holder = state.repositories.get(handle)?;
    repository::set_read_access(&holder.repository, local_read_password, share_token).await?;
    Ok(())
}

pub(crate) async fn set_read_and_write_access(
    state: &State,
    handle: RepositoryHandle,
    local_old_rw_password: Option<String>,
    local_new_rw_password: Option<String>,
    share_token: Option<ShareToken>,
) -> Result<(), Error> {
    let holder = state.repositories.get(handle)?;
    repository::set_read_and_write_access(
        &holder.repository,
        local_old_rw_password,
        local_new_rw_password,
        share_token,
    )
    .await?;
    Ok(())
}

/// Note that after removing read key the user may still read the repository if they previously had
/// write key set up.
pub(crate) async fn remove_read_key(state: &State, handle: RepositoryHandle) -> Result<(), Error> {
    state
        .repositories
        .get(handle)?
        .repository
        .remove_read_key()
        .await?;
    Ok(())
}

/// Note that removing the write key will leave read key intact.
pub(crate) async fn remove_write_key(state: &State, handle: RepositoryHandle) -> Result<(), Error> {
    state
        .repositories
        .get(handle)?
        .repository
        .remove_write_key()
        .await?;
    Ok(())
}

/// Returns true if the repository requires a local password to be opened for reading.
pub(crate) async fn requires_local_password_for_reading(
    state: &State,
    handle: RepositoryHandle,
) -> Result<bool, Error> {
    Ok(state
        .repositories
        .get(handle)?
        .repository
        .requires_local_password_for_reading()
        .await?)
}

/// Returns true if the repository requires a local password to be opened for writing.
pub(crate) async fn requires_local_password_for_writing(
    state: &State,
    handle: RepositoryHandle,
) -> Result<bool, Error> {
    Ok(state
        .repositories
        .get(handle)?
        .repository
        .requires_local_password_for_writing()
        .await?)
}

/// Return the info-hash of the repository formatted as hex string. This can be used as a globally
/// unique, non-secret identifier of the repository.
/// User is responsible for deallocating the returned string.
pub(crate) fn info_hash(state: &State, handle: RepositoryHandle) -> Result<String, Error> {
    let holder = state.repositories.get(handle)?;
    let info_hash = network::repository_info_hash(holder.repository.secrets().id());

    Ok(hex::encode(info_hash))
}

/// Returns an ID that is randomly generated once per repository. Can be used to store local user
/// data per repository (e.g. passwords behind biometric storage).
pub(crate) async fn database_id(state: &State, handle: RepositoryHandle) -> Result<Vec<u8>, Error> {
    let holder = state.repositories.get(handle)?;
    Ok(holder.repository.database_id().await?.as_ref().to_vec())
}

/// Returns the type of repository entry (file, directory, ...) or `None` if the entry doesn't
/// exist.
pub(crate) async fn entry_type(
    state: &State,
    handle: RepositoryHandle,
    path: Utf8PathBuf,
) -> Result<Option<u8>, Error> {
    let holder = state.repositories.get(handle)?;

    match holder.repository.lookup_type(path).await {
        Ok(entry_type) => Ok(Some(entry_type.into())),
        Err(ouisync_lib::Error::EntryNotFound) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Move/rename entry from src to dst.
pub(crate) async fn move_entry(
    state: &State,
    handle: RepositoryHandle,
    src: Utf8PathBuf,
    dst: Utf8PathBuf,
) -> Result<(), Error> {
    let holder = state.repositories.get(handle)?;
    let (src_dir, src_name) = path::decompose(&src).ok_or(ouisync_lib::Error::EntryNotFound)?;
    let (dst_dir, dst_name) = path::decompose(&dst).ok_or(ouisync_lib::Error::EntryNotFound)?;

    holder
        .repository
        .move_entry(src_dir, src_name, dst_dir, dst_name)
        .await?;

    Ok(())
}

/// Subscribe to change notifications from the repository.
pub(crate) fn subscribe(
    state: &State,
    notification_tx: &NotificationSender,
    repository_handle: RepositoryHandle,
) -> Result<TaskHandle, Error> {
    let holder = state.repositories.get(repository_handle)?;

    let mut notification_rx = holder.repository.subscribe();
    let notification_tx = notification_tx.clone();

    let handle = state.spawn_task(|id| async move {
        loop {
            match notification_rx.recv().await {
                Ok(Event {
                    payload: Payload::BranchChanged(_) | Payload::BlockReceived { .. },
                    ..
                }) => (),
                Ok(Event { .. }) => continue,
                Err(RecvError::Lagged(_)) => (),
                Err(RecvError::Closed) => break,
            }

            notification_tx
                .send((id, Notification::Repository))
                .await
                .ok();
        }
    });

    Ok(handle)
}

pub(crate) fn is_dht_enabled(state: &State, handle: RepositoryHandle) -> Result<bool, Error> {
    Ok(state
        .repositories
        .get(handle)?
        .registration
        .is_dht_enabled())
}

pub(crate) async fn set_dht_enabled(
    state: &State,
    handle: RepositoryHandle,
    enabled: bool,
) -> Result<(), Error> {
    state
        .repositories
        .get(handle)?
        .registration
        .set_dht_enabled(enabled)
        .await;
    Ok(())
}

pub(crate) fn is_pex_enabled(state: &State, handle: RepositoryHandle) -> Result<bool, Error> {
    Ok(state
        .repositories
        .get(handle)?
        .registration
        .is_pex_enabled())
}

pub(crate) async fn set_pex_enabled(
    state: &State,
    handle: RepositoryHandle,
    enabled: bool,
) -> Result<(), Error> {
    state
        .repositories
        .get(handle)?
        .registration
        .set_pex_enabled(enabled)
        .await;
    Ok(())
}

/// The `password` parameter is optional, if `None` the current access level of the opened
/// repository is used. If provided, the highest access level that the password can unlock is used.
pub(crate) async fn create_share_token(
    state: &State,
    repository: RepositoryHandle,
    password: Option<String>,
    access_mode: AccessMode,
    name: Option<String>,
) -> Result<String, Error> {
    let holder = state.repositories.get(repository)?;
    let token =
        repository::create_share_token(&holder.repository, password, access_mode, name).await?;
    Ok(token)
}

pub(crate) fn access_mode(state: &State, handle: RepositoryHandle) -> Result<u8, Error> {
    Ok(state
        .repositories
        .get(handle)?
        .repository
        .access_mode()
        .into())
}

/// Returns the syncing progress.
pub(crate) async fn sync_progress(
    state: &State,
    handle: RepositoryHandle,
) -> Result<Progress, Error> {
    Ok(state
        .repositories
        .get(handle)?
        .repository
        .sync_progress()
        .await?)
}

/// Mirror the repository to the storage servers
pub(crate) async fn mirror(state: &State, handle: RepositoryHandle) -> Result<(), Error> {
    let holder = state.repositories.get(handle)?;
    let config = state.get_remote_client_config()?;
    let hosts: Vec<_> = state
        .cache_servers
        .lock()
        .unwrap()
        .iter()
        .cloned()
        .collect();

    ouisync_bridge::repository::mirror(&holder.repository, config, &hosts).await?;

    Ok(())
}

/// Mount all opened repositories
pub(crate) async fn mount_all(state: &State, mount_point: PathBuf) -> Result<(), Error> {
    let repos = state.repositories.collect();
    state
        .mounter
        .mount_all(
            mount_point,
            repos
                .iter()
                .map(|holder| (holder.store_path.as_ref(), &holder.repository)),
        )
        .await?;

    Ok(())
}

/// Registry of opened repositories.
pub(crate) struct Repositories {
    inner: RwLock<Inner>,
}

impl Repositories {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(Inner {
                registry: Registry::new(),
                index: HashMap::new(),
            }),
        }
    }

    /// Gets or inserts a repository.
    pub async fn entry(&self, store_path: PathBuf) -> RepositoryEntry {
        loop {
            let notify = {
                let mut inner = self.inner.write().unwrap();

                match inner.index.entry(store_path.clone()) {
                    Entry::Occupied(entry) => match entry.get() {
                        IndexEntry::Reserved(notify) => {
                            // The repo doesn't exists yet but someone is already inserting it.
                            notify.clone()
                        }
                        IndexEntry::Existing(handle) => {
                            // The repo already exists.
                            return RepositoryEntry::Occupied(*handle);
                        }
                    },
                    Entry::Vacant(entry) => {
                        entry.insert(IndexEntry::Reserved(Arc::new(Notify::new())));

                        // The repo doesn't exist yet and we are the first one to insert it.
                        return RepositoryEntry::Vacant(RepositoryVacantEntry {
                            inner: &self.inner,
                            store_path,
                            inserted: false,
                        });
                    }
                }
            };

            notify.notified().await;
        }
    }

    /// Removes the repository regardless of how many handles it has. All outstanding handles
    /// become invalid.
    pub fn remove(&self, handle: RepositoryHandle) -> Option<Arc<RepositoryHolder>> {
        let mut inner = self.inner.write().unwrap();

        let holder = inner.registry.remove(handle)?;
        inner.index.remove(&holder.store_path);

        Some(holder)
    }

    pub fn get(&self, handle: RepositoryHandle) -> Result<Arc<RepositoryHolder>, InvalidHandle> {
        self.inner
            .read()
            .unwrap()
            .registry
            .get(handle)
            .map(|holder| holder.clone())
    }

    pub fn collect(&self) -> Vec<Arc<RepositoryHolder>> {
        self.inner
            .read()
            .unwrap()
            .registry
            .values()
            .cloned()
            .collect()
    }
}

pub(crate) enum RepositoryEntry<'a> {
    Occupied(RepositoryHandle),
    Vacant(RepositoryVacantEntry<'a>),
}

pub(crate) struct RepositoryVacantEntry<'a> {
    inner: &'a RwLock<Inner>,
    store_path: PathBuf,
    inserted: bool,
}

impl RepositoryVacantEntry<'_> {
    pub fn insert(mut self, holder: RepositoryHolder) -> RepositoryHandle {
        let mut inner = self.inner.write().unwrap();

        let handle = inner.registry.insert(Arc::new(holder));

        let Some(entry) = inner.index.get_mut(&self.store_path) else {
            unreachable!()
        };

        let IndexEntry::Reserved(notify) = mem::replace(entry, IndexEntry::Existing(handle)) else {
            unreachable!()
        };

        self.inserted = true;

        notify.notify_waiters();

        handle
    }
}

impl Drop for RepositoryVacantEntry<'_> {
    fn drop(&mut self) {
        if self.inserted {
            return;
        }

        let mut inner = self.inner.write().unwrap();

        let Some(IndexEntry::Reserved(notify)) = inner.index.remove(&self.store_path) else {
            unreachable!()
        };

        notify.notify_waiters();
    }
}

struct Inner {
    // Registry of the repos
    registry: Registry<Arc<RepositoryHolder>>,
    // Index for looking up repos by their store paths.
    index: HashMap<PathBuf, IndexEntry>,
}

enum IndexEntry {
    Reserved(Arc<Notify>),
    Existing(RepositoryHandle),
}
