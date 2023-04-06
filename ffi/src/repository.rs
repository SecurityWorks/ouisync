use crate::{
    registry::Handle,
    state::{State, SubscriptionHandle},
};
use camino::Utf8PathBuf;
use ouisync_bridge::{
    constants::{ENTRY_TYPE_DIRECTORY, ENTRY_TYPE_FILE},
    error::Result,
    protocol::Notification,
    repository::{self, RepositoryHolder},
    transport::NotificationSender,
};
use ouisync_lib::{network, path, AccessMode, EntryType, Event, Payload, Progress, ShareToken};
use tokio::sync::broadcast::error::RecvError;

pub(crate) async fn create(
    state: &State,
    store: Utf8PathBuf,
    local_read_password: Option<String>,
    local_write_password: Option<String>,
    share_token: Option<ShareToken>,
) -> Result<Handle<RepositoryHolder>> {
    let holder = repository::create(
        store,
        local_read_password,
        local_write_password,
        share_token,
        &state.config,
        &state.network,
        &state.repos_monitor,
    )
    .await?;
    let handle = state.repositories.insert(holder);

    Ok(handle)
}

/// Opens an existing repository.
pub(crate) async fn open(
    state: &State,
    store: Utf8PathBuf,
    local_password: Option<String>,
) -> Result<Handle<RepositoryHolder>> {
    let holder = repository::open(
        store,
        local_password,
        &state.config,
        &state.network,
        &state.repos_monitor,
    )
    .await?;
    let handle = state.repositories.insert(holder);

    Ok(handle)
}

/// Closes a repository.
pub(crate) async fn close(state: &State, handle: Handle<RepositoryHolder>) -> Result<()> {
    let holder = state.repositories.remove(handle);

    if let Some(holder) = holder {
        holder.repository.close().await?
    }

    Ok(())
}

pub(crate) fn create_reopen_token(
    state: &State,
    handle: Handle<RepositoryHolder>,
) -> Result<Vec<u8>> {
    let holder = state.repositories.get(handle);
    let token = holder.repository.reopen_token().encode();

    Ok(token)
}

pub(crate) async fn reopen(
    state: &State,
    store: Utf8PathBuf,
    token: Vec<u8>,
) -> Result<Handle<RepositoryHolder>> {
    let holder = repository::reopen(store, token, &state.network, &state.repos_monitor).await?;
    let handle = state.repositories.insert(holder);

    Ok(handle)
}

pub(crate) async fn set_read_access(
    state: &State,
    handle: Handle<RepositoryHolder>,
    local_read_password: Option<String>,
    share_token: Option<ShareToken>,
) -> Result<()> {
    let holder = state.repositories.get(handle);
    repository::set_read_access(&holder.repository, local_read_password, share_token).await
}

pub(crate) async fn set_read_and_write_access(
    state: &State,
    handle: Handle<RepositoryHolder>,
    local_old_rw_password: Option<String>,
    local_new_rw_password: Option<String>,
    share_token: Option<ShareToken>,
) -> Result<()> {
    let holder = state.repositories.get(handle);
    repository::set_read_and_write_access(
        &holder.repository,
        local_old_rw_password,
        local_new_rw_password,
        share_token,
    )
    .await
}

/// Note that after removing read key the user may still read the repository if they previously had
/// write key set up.
pub(crate) async fn remove_read_key(state: &State, handle: Handle<RepositoryHolder>) -> Result<()> {
    state
        .repositories
        .get(handle)
        .repository
        .remove_read_key()
        .await?;
    Ok(())
}

/// Note that removing the write key will leave read key intact.
pub(crate) async fn remove_write_key(
    state: &State,
    handle: Handle<RepositoryHolder>,
) -> Result<()> {
    state
        .repositories
        .get(handle)
        .repository
        .remove_write_key()
        .await?;
    Ok(())
}

/// Returns true if the repository requires a local password to be opened for reading.
pub(crate) async fn requires_local_password_for_reading(
    state: &State,
    handle: Handle<RepositoryHolder>,
) -> Result<bool> {
    Ok(state
        .repositories
        .get(handle)
        .repository
        .requires_local_password_for_reading()
        .await?)
}

/// Returns true if the repository requires a local password to be opened for writing.
pub(crate) async fn requires_local_password_for_writing(
    state: &State,
    handle: Handle<RepositoryHolder>,
) -> Result<bool> {
    Ok(state
        .repositories
        .get(handle)
        .repository
        .requires_local_password_for_writing()
        .await?)
}

/// Return the info-hash of the repository formatted as hex string. This can be used as a globally
/// unique, non-secret identifier of the repository.
/// User is responsible for deallocating the returned string.
pub(crate) fn info_hash(state: &State, handle: Handle<RepositoryHolder>) -> String {
    let holder = state.repositories.get(handle);
    let info_hash = network::repository_info_hash(holder.repository.secrets().id());

    hex::encode(info_hash)
}

/// Returns an ID that is randomly generated once per repository. Can be used to store local user
/// data per repository (e.g. passwords behind biometric storage).
pub(crate) async fn database_id(
    state: &State,
    handle: Handle<RepositoryHolder>,
) -> Result<Vec<u8>> {
    let holder = state.repositories.get(handle);
    Ok(holder.repository.database_id().await?.as_ref().to_vec())
}

/// Returns the type of repository entry (file, directory, ...) or `None` if the entry doesn't
/// exist.
pub(crate) async fn entry_type(
    state: &State,
    handle: Handle<RepositoryHolder>,
    path: Utf8PathBuf,
) -> Result<Option<u8>> {
    let holder = state.repositories.get(handle);

    match holder.repository.lookup_type(path).await {
        Ok(entry_type) => Ok(Some(entry_type_to_num(entry_type))),
        Err(ouisync_lib::Error::EntryNotFound) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Move/rename entry from src to dst.
pub(crate) async fn move_entry(
    state: &State,
    handle: Handle<RepositoryHolder>,
    src: Utf8PathBuf,
    dst: Utf8PathBuf,
) -> Result<()> {
    let holder = state.repositories.get(handle);
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
    repository_handle: Handle<RepositoryHolder>,
) -> SubscriptionHandle {
    let holder = state.repositories.get(repository_handle);

    let mut notification_rx = holder.repository.subscribe();
    let notification_tx = notification_tx.clone();

    let entry = state.tasks.vacant_entry();
    let subscription_id = entry.handle().id();

    let subscription_task = scoped_task::spawn(async move {
        loop {
            match notification_rx.recv().await {
                // Only `BlockReceived` events cause user-observable changes
                Ok(Event {
                    payload: Payload::BlockReceived { .. },
                    ..
                }) => (),
                Ok(Event { .. }) => continue,
                Err(RecvError::Lagged(_)) => (),
                Err(RecvError::Closed) => break,
            }

            notification_tx
                .send((subscription_id, Notification::Repository))
                .await
                .ok();
        }
    });

    entry.insert(subscription_task)
}

pub(crate) fn is_dht_enabled(state: &State, handle: Handle<RepositoryHolder>) -> bool {
    state.repositories.get(handle).registration.is_dht_enabled()
}

pub(crate) async fn set_dht_enabled(
    state: &State,
    handle: Handle<RepositoryHolder>,
    enabled: bool,
) {
    let reg = &state.repositories.get(handle).registration;
    reg.set_dht_enabled(enabled).await
}

pub(crate) fn is_pex_enabled(state: &State, handle: Handle<RepositoryHolder>) -> bool {
    state.repositories.get(handle).registration.is_pex_enabled()
}

pub(crate) async fn set_pex_enabled(
    state: &State,
    handle: Handle<RepositoryHolder>,
    enabled: bool,
) {
    let reg = &state.repositories.get(handle).registration;
    reg.set_pex_enabled(enabled).await
}

/// The `password` parameter is optional, if `None` the current access level of the opened
/// repository is used. If provided, the highest access level that the password can unlock is used.
pub(crate) async fn create_share_token(
    state: &State,
    repository: Handle<RepositoryHolder>,
    password: Option<String>,
    access_mode: AccessMode,
    name: Option<String>,
) -> Result<String> {
    let holder = state.repositories.get(repository);
    repository::create_share_token(&holder.repository, password, access_mode, name).await
}

pub(crate) fn access_mode(state: &State, handle: Handle<RepositoryHolder>) -> u8 {
    state
        .repositories
        .get(handle)
        .repository
        .access_mode()
        .into()
}

/// Returns the syncing progress.
pub(crate) async fn sync_progress(
    state: &State,
    handle: Handle<RepositoryHolder>,
) -> Result<Progress> {
    Ok(state
        .repositories
        .get(handle)
        .repository
        .sync_progress()
        .await?)
}

pub(crate) fn entry_type_to_num(entry_type: EntryType) -> u8 {
    match entry_type {
        EntryType::File => ENTRY_TYPE_FILE,
        EntryType::Directory => ENTRY_TYPE_DIRECTORY,
    }
}
