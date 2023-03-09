use crate::{
    directory, file, network,
    protocol::{Request, Response},
    repository, share_token,
    state::State,
    state_monitor,
};
use async_trait::async_trait;
use ouisync_bridge::{error::Result, transport::NotificationSender};
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct Handler {
    state: Arc<State>,
}

impl Handler {
    pub fn new(state: Arc<State>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl ouisync_bridge::transport::Handler for Handler {
    type Request = Request;
    type Response = Response;

    async fn handle(
        &self,
        request: Self::Request,
        notification_tx: &NotificationSender,
    ) -> Result<Self::Response> {
        let response = match request {
            Request::RepositoryCreate {
                path,
                read_password,
                write_password,
                share_token,
            } => repository::create(
                &self.state,
                path,
                read_password,
                write_password,
                share_token,
            )
            .await?
            .into(),
            Request::RepositoryOpen { path, password } => {
                repository::open(&self.state, path, password).await?.into()
            }
            Request::RepositoryClose(handle) => {
                repository::close(&self.state, handle).await?.into()
            }
            Request::RepositoryCreateReopenToken(handle) => {
                repository::create_reopen_token(&self.state, handle)?.into()
            }
            Request::RepositoryReopen { path, token } => {
                repository::reopen(&self.state, path, token).await?.into()
            }
            Request::RepositorySubscribe(handle) => {
                repository::subscribe(&self.state, notification_tx, handle).into()
            }
            Request::RepositorySetReadAccess {
                repository,
                password,
                share_token,
            } => repository::set_read_access(&self.state, repository, password, share_token)
                .await?
                .into(),
            Request::RepositorySetReadAndWriteAccess {
                repository,
                old_password,
                new_password,
                share_token,
            } => repository::set_read_and_write_access(
                &self.state,
                repository,
                old_password,
                new_password,
                share_token,
            )
            .await?
            .into(),
            Request::RepositoryRemoveReadKey(handle) => {
                repository::remove_read_key(&self.state, handle)
                    .await?
                    .into()
            }
            Request::RepositoryRemoveWriteKey(handle) => {
                repository::remove_write_key(&self.state, handle)
                    .await?
                    .into()
            }
            Request::RepositoryRequiresLocalPasswordForReading(handle) => {
                repository::requires_local_password_for_reading(&self.state, handle)
                    .await?
                    .into()
            }
            Request::RepositoryRequiresLocalPasswordForWriting(handle) => {
                repository::requires_local_password_for_writing(&self.state, handle)
                    .await?
                    .into()
            }
            Request::RepositoryInfoHash(handle) => {
                repository::info_hash(&self.state, handle).into()
            }
            Request::RepositoryDatabaseId(handle) => {
                repository::database_id(&self.state, handle).await?.into()
            }
            Request::RepositoryEntryType { repository, path } => {
                repository::entry_type(&self.state, repository, path)
                    .await?
                    .into()
            }
            Request::RepositoryMoveEntry {
                repository,
                src,
                dst,
            } => repository::move_entry(&self.state, repository, src, dst)
                .await?
                .into(),
            Request::RepositoryIsDhtEnabled(repository) => {
                repository::is_dht_enabled(&self.state, repository).into()
            }
            Request::RepositorySetDhtEnabled {
                repository,
                enabled,
            } => {
                repository::set_dht_enabled(&self.state, repository, enabled);
                ().into()
            }
            Request::RepositoryIsPexEnabled(repository) => {
                repository::is_pex_enabled(&self.state, repository).into()
            }
            Request::RepositorySetPexEnabled {
                repository,
                enabled,
            } => {
                repository::set_pex_enabled(&self.state, repository, enabled);
                ().into()
            }
            Request::RepositoryCreateShareToken {
                repository,
                password,
                access_mode,
                name,
            } => {
                repository::create_share_token(&self.state, repository, password, access_mode, name)
                    .await?
                    .into()
            }
            Request::ShareTokenMode(token) => share_token::mode(token).into(),
            Request::ShareTokenInfoHash(token) => share_token::info_hash(token).into(),
            Request::ShareTokenSuggestedName(token) => share_token::suggested_name(token).into(),
            Request::ShareTokenNormalize(token) => token.to_string().into(),
            Request::RepositoryAccessMode(repository) => {
                repository::access_mode(&self.state, repository).into()
            }
            Request::RepositorySyncProgress(repository) => {
                repository::sync_progress(&self.state, repository)
                    .await?
                    .into()
            }
            Request::DirectoryCreate { repository, path } => {
                directory::create(&self.state, repository, path)
                    .await?
                    .into()
            }
            Request::DirectoryOpen { repository, path } => {
                directory::open(&self.state, repository, path).await?.into()
            }
            Request::DirectoryRemove {
                repository,
                path,
                recursive,
            } => directory::remove(&self.state, repository, path, recursive)
                .await?
                .into(),
            Request::FileOpen { repository, path } => {
                file::open(&self.state, repository, path).await?.into()
            }
            Request::FileCreate { repository, path } => {
                file::create(&self.state, repository, path).await?.into()
            }
            Request::FileRemove { repository, path } => {
                file::remove(&self.state, repository, path).await?.into()
            }
            Request::FileRead { file, offset, len } => {
                file::read(&self.state, file, offset, len).await?.into()
            }
            Request::FileWrite { file, offset, data } => {
                file::write(&self.state, file, offset, data).await?.into()
            }
            Request::FileTruncate { file, len } => {
                file::truncate(&self.state, file, len).await?.into()
            }
            Request::FileLen(file) => file::len(&self.state, file).await.into(),
            Request::FileFlush(file) => file::flush(&self.state, file).await?.into(),
            Request::FileClose(file) => file::close(&self.state, file).await?.into(),
            Request::NetworkSubscribe => network::subscribe(&self.state, notification_tx).into(),
            Request::NetworkBind {
                quic_v4,
                quic_v6,
                tcp_v4,
                tcp_v6,
            } => {
                ouisync_bridge::network::bind(
                    &self.state.network,
                    quic_v4,
                    quic_v6,
                    tcp_v4,
                    tcp_v6,
                )
                .await;
                ().into()
            }
            Request::NetworkTcpListenerLocalAddrV4 => {
                self.state.network.tcp_listener_local_addr_v4().into()
            }
            Request::NetworkTcpListenerLocalAddrV6 => {
                self.state.network.tcp_listener_local_addr_v6().into()
            }
            Request::NetworkQuicListenerLocalAddrV4 => {
                self.state.network.quic_listener_local_addr_v4().into()
            }
            Request::NetworkQuicListenerLocalAddrV6 => {
                self.state.network.quic_listener_local_addr_v6().into()
            }
            Request::NetworkAddUserProvidedPeer(addr) => {
                self.state.network.add_user_provided_peer(&addr);
                ().into()
            }
            Request::NetworkRemoveUserProvidedPeer(addr) => {
                self.state.network.remove_user_provided_peer(&addr);
                ().into()
            }
            Request::NetworkKnownPeers => self.state.network.collect_peer_info().into(),
            Request::NetworkThisRuntimeId => network::this_runtime_id(&self.state).into(),
            Request::NetworkCurrentProtocolVersion => {
                self.state.network.current_protocol_version().into()
            }
            Request::NetworkHighestSeenProtocolVersion => {
                self.state.network.highest_seen_protocol_version().into()
            }
            Request::NetworkIsPortForwardingEnabled => {
                self.state.network.is_port_forwarding_enabled().into()
            }
            Request::NetworkSetPortForwardingEnabled(enabled) => {
                ouisync_bridge::network::set_port_forwarding_enabled(&self.state.network, enabled);
                ().into()
            }
            Request::NetworkIsLocalDiscoveryEnabled => {
                self.state.network.is_local_discovery_enabled().into()
            }
            Request::NetworkSetLocalDiscoveryEnabled(enabled) => {
                ouisync_bridge::network::set_local_discovery_enabled(&self.state.network, enabled);
                ().into()
            }
            Request::NetworkShutdown => {
                self.state.network.handle().shutdown().await;
                ().into()
            }
            Request::StateMonitorGet(path) => state_monitor::get(&self.state, path)?.into(),
            Request::StateMonitorSubscribe(path) => {
                state_monitor::subscribe(&self.state, notification_tx, path)?.into()
            }
            Request::Unsubscribe(handle) => {
                self.state.unsubscribe(handle);
                ().into()
            }
        };

        Ok(response)
    }
}
