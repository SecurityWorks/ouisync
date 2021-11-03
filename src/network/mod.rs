mod client;
mod connection;
mod message;
mod message_broker;
mod object_stream;
mod local_discovery;
mod server;
#[cfg(test)]
mod tests;

use self::{
    connection::{ConnectionDeduplicator, ConnectionDirection, ConnectionPermit},
    message::RepositoryId,
    message_broker::MessageBroker,
    object_stream::TcpObjectStream,
    local_discovery::LocalDiscovery,
};
use crate::{
    crypto::Hashable,
    error::{Error, Result},
    index::Index,
    replica_id::ReplicaId,
    repository::Repository,
    scoped_task::{ScopedJoinHandle, ScopedTaskSet},
    tagged::{Local, Remote},
    upnp,
};
use btdht::{DhtEvent, InfoHash, MainlineDht, INFO_HASH_LEN};
use futures_util::future;
use rand::Rng;
use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    convert::TryFrom,
    fmt, io, iter,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{Arc, Weak},
    time::Duration,
};
use structopt::StructOpt;
use tokio::{
    net::{self, TcpListener, TcpStream, UdpSocket},
    select,
    sync::{mpsc, Mutex, RwLock},
    task,
    time,
};

// Hardcoded DHT routers to bootstrap the DHT against.
// TODO: add this to `NetworkOptions` so it can be overriden by the user.
const DHT_ROUTERS: &[&str] = &["router.bittorrent.com:6881", "dht.transmissionbt.com:6881"];

// Interval for the delay before a repository is re-announced on the DHT. The actual delay is an
// uniformly random value from this interval.
// BEP5 indicatest that "After 15 minutes of inactivity, a node becomes questionable." so try not
// to get too close to that value to avoid DHT churn.
const MIN_DHT_ANNOUNCE_DELAY: Duration = Duration::from_secs(5 * 60);
const MAX_DHT_ANNOUNCE_DELAY: Duration = Duration::from_secs(12 * 60);

#[derive(StructOpt, Debug)]
pub struct NetworkOptions {
    /// Port to listen on (0 for random)
    #[structopt(short, long, default_value = "0")]
    pub port: u16,

    /// IP address to bind to
    #[structopt(short, long, default_value = "0.0.0.0", value_name = "ip")]
    pub bind: IpAddr,

    /// Disable local discovery
    #[structopt(short, long)]
    pub disable_local_discovery: bool,

    /// Disable UPnP
    #[structopt(long)]
    pub disable_upnp: bool,

    /// Disable DHT
    #[structopt(long)]
    pub disable_dht: bool,

    /// Explicit list of IP:PORT pairs of peers to connect to
    #[structopt(long)]
    pub peers: Vec<SocketAddr>,
}

impl NetworkOptions {
    pub fn listen_addr(&self) -> SocketAddr {
        SocketAddr::new(self.bind, self.port)
    }
}

impl Default for NetworkOptions {
    fn default() -> Self {
        Self {
            port: 0,
            bind: Ipv4Addr::UNSPECIFIED.into(),
            disable_local_discovery: false,
            disable_upnp: false,
            disable_dht: false,
            peers: Vec::new(),
        }
    }
}

pub struct Network {
    inner: Arc<Inner>,
    // We keep tasks here instead of in Inner because we want them to be
    // destroyed when Network is Dropped.
    _tasks: Arc<RwLock<Tasks>>,
    _port_forwarder: Option<upnp::PortForwarder>,
}

impl Network {
    pub async fn new(this_replica_id: ReplicaId, options: &NetworkOptions) -> Result<Self> {
        let listener = TcpListener::bind(options.listen_addr())
            .await
            .map_err(Error::Network)?;

        let local_addr = listener.local_addr().map_err(Error::Network)?;

        let dht_socket = if !options.disable_dht {
            Some(
                UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
                    .await
                    .map_err(Error::Network)?,
            )
        } else {
            None
        };
        let dht_port = dht_socket
            .as_ref()
            .map(|socket| socket.local_addr())
            .transpose()
            .map_err(Error::Network)?
            .map(|addr| addr.port());

        let port_forwarder = if !options.disable_upnp {
            Some(upnp::PortForwarder::new(
                iter::once(upnp::Mapping {
                    external: local_addr.port(),
                    internal: local_addr.port(),
                    protocol: upnp::Protocol::Tcp,
                })
                .chain(dht_port.map(|port| upnp::Mapping {
                    external: port,
                    internal: port,
                    protocol: upnp::Protocol::Udp,
                })),
            ))
        } else {
            None
        };

        let (dht, dht_event_rx) = if let Some(socket) = dht_socket {
            // TODO: load the DHT state from a previous save if it exists.
            let (dht, event_rx) = MainlineDht::builder()
                .add_routers(dht_router_addresses().await)
                .set_read_only(false)
                .set_announce_port(local_addr.port())
                .start(socket);

            (Some(dht), Some(event_rx))
        } else {
            (None, None)
        };

        let tasks = Arc::new(RwLock::new(Tasks::default()));

        let inner = Inner {
            local_addr,
            this_replica_id,
            message_brokers: Mutex::new(HashMap::new()),
            indices: RwLock::new(IndexMap::default()),
            dht,
            connection_deduplicator: ConnectionDeduplicator::new(),
            tasks: Arc::downgrade(&tasks),
        };

        let network = Self {
            inner: Arc::new(inner),
            _tasks: tasks,
            _port_forwarder: port_forwarder,
        };

        network.inner.start_listener(listener).await;
        network.inner.enable_local_discovery(!options.disable_local_discovery).await;

        for peer in &options.peers {
            network.inner.clone().establish_user_provided_connection(*peer).await;
        }

        if let Some(event_rx) = dht_event_rx {
            network.inner.clone().start_dht(event_rx).await;
        }

        Ok(network)
    }

    pub fn local_addr(&self) -> &SocketAddr {
        &self.inner.local_addr
    }

    pub fn handle(&self) -> Handle {
        Handle {
            inner: self.inner.clone(),
        }
    }
}

/// Handle for the network which can be cheaply cloned and sent to other threads.
#[derive(Clone)]
pub struct Handle {
    inner: Arc<Inner>,
}

impl Handle {
    /// Register a local repository into the network. This links the repository with all matching
    /// repositories of currently connected remote replicas as well as any replicas connected in
    /// the future. The repository is automatically deregistered when dropped.
    pub async fn register(&self, name: &str, repository: &Repository) -> bool {
        let index = repository.index();

        let id = if let Some(id) = self
            .inner
            .indices
            .write()
            .await
            .insert(name.to_owned(), index.clone())
        {
            id
        } else {
            return false;
        };

        for broker in self.inner.message_brokers.lock().await.values() {
            create_link(broker, id, name.to_owned(), index.clone()).await;
        }

        let tasks_arc = self.inner.tasks.upgrade().unwrap();
        let tasks = tasks_arc.write().await;

        // Deregister the index when it gets closed.
        tasks.other.spawn({
            let closed = index.subscribe().closed();
            let inner = self.inner.clone();

            async move {
                closed.await;

                inner.indices.write().await.remove(id);

                for broker in inner.message_brokers.lock().await.values() {
                    broker.destroy_link(Local::new(id)).await;
                }
            }
        });

        self.inner.find_peers_for_repository(name, index).await;

        true
    }

    pub fn this_replica_id(&self) -> &ReplicaId {
        &self.inner.this_replica_id
    }

    pub async fn is_local_discovery_enabled(&self) -> bool {
        let tasks_arc = self.inner.tasks.upgrade().unwrap();
        let tasks = tasks_arc.read().await;
        tasks.local_discovery.is_some()
    }

    pub async fn enable_local_discovery(&self, enable: bool) {
        self.inner.enable_local_discovery(enable).await;
    }
}

#[derive(Clone, Copy, Debug)]
enum PeerSource {
    UserProvided,
    Listener,
    LocalDiscovery,
    Dht,
}

impl fmt::Display for PeerSource {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            PeerSource::Listener => write!(f, "incoming"),
            PeerSource::UserProvided => write!(f, "outgoing (user provided)"),
            PeerSource::LocalDiscovery => write!(f, "outgoing (locally discovered)"),
            PeerSource::Dht => write!(f, "outgoing (found via DHT)"),
        }
    }
}

#[derive(Default)]
struct Tasks {
    local_discovery: Option<ScopedJoinHandle<()>>,
    other: ScopedTaskSet,
}

struct Inner {
    local_addr: SocketAddr,
    this_replica_id: ReplicaId,
    message_brokers: Mutex<HashMap<ReplicaId, MessageBroker>>,
    indices: RwLock<IndexMap>,
    dht: Option<MainlineDht>,
    connection_deduplicator: ConnectionDeduplicator,
    // Note that unwrapping the upgraded weak pointer should be fine because if the underlying Arc
    // was Dropped, we would not be askinf ro the upgrade in the first place.
    tasks: Weak<RwLock<Tasks>>,
}

impl Inner {
    async fn enable_local_discovery(self: &Arc<Self>, enable: bool) {
        let tasks_arc = self.tasks.upgrade().unwrap();
        let mut tasks = tasks_arc.write().await;

        if !enable {
            tasks.local_discovery = None;
            return;
        }

        if tasks.local_discovery.is_some() {
            return;
        }

        let self_ = self.clone();
        tasks.local_discovery = Some(ScopedJoinHandle(task::spawn(async move {
            let port = self_.local_addr.port();
            self_.run_local_discovery(port).await;
        })));
    }

    async fn run_local_discovery(self: Arc<Self>, listener_port: u16) {
        let discovery = match LocalDiscovery::new(listener_port) {
            Ok(discovery) => discovery,
            Err(error) => {
                log::error!("Failed to create LocalDiscovery: {}", error);
                return;
            }
        };

        while let Some(addr) = discovery.recv().await {
            let tasks_arc = self.tasks.upgrade().unwrap();
            let tasks = tasks_arc.write().await;

            tasks.other
                .spawn(self.clone().establish_discovered_connection(addr))
        }
    }

    async fn start_listener(self: &Arc<Self>, listener: TcpListener) {
        let tasks_arc = self.tasks.upgrade().unwrap();
        let tasks = tasks_arc.write().await;
        tasks.other.spawn(self.clone().run_listener(listener));
    }

    async fn run_listener(self: Arc<Self>, listener: TcpListener) {
        loop {
            let (socket, addr) = match listener.accept().await {
                Ok(pair) => pair,
                Err(error) => {
                    log::error!("Failed to accept incoming TCP connection: {}", error);
                    break;
                }
            };

            if let Some(permit) = self
                .connection_deduplicator
                .reserve(addr, ConnectionDirection::Incoming)
            {
                let tasks_arc = self.tasks.upgrade().unwrap();
                let tasks = tasks_arc.write().await;

                tasks.other.spawn(self.clone().handle_new_connection(
                    socket,
                    PeerSource::Listener,
                    permit,
                ))
            }
        }
    }

    async fn start_dht(self: Arc<Self>, event_rx: mpsc::UnboundedReceiver<DhtEvent>) {
        let tasks_arc = self.tasks.upgrade().unwrap();
        let tasks = tasks_arc.write().await;
        tasks.other.spawn(self.run_dht(event_rx));
    }

    async fn run_dht(self: Arc<Self>, mut event_rx: mpsc::UnboundedReceiver<DhtEvent>) {
        // To deduplicate found peers.
        let mut lookups: HashMap<_, HashSet<_>> = HashMap::new();

        while let Some(event) = event_rx.recv().await {
            match event {
                DhtEvent::BootstrapCompleted => log::info!("DHT bootstrap complete"),
                DhtEvent::BootstrapFailed => {
                    log::error!("DHT bootstrap failed");
                    break;
                }
                DhtEvent::PeerFound(info_hash, addr) => {
                    log::debug!("DHT found peer for {:?}: {}", info_hash, addr);
                    lookups.entry(info_hash).or_default().insert(addr);
                }
                DhtEvent::LookupCompleted(info_hash) => {
                    log::debug!("DHT lookup for {:?} complete", info_hash);

                    let tasks_arc = self.tasks.upgrade().unwrap();
                    let tasks = tasks_arc.write().await;

                    for addr in lookups.remove(&info_hash).unwrap_or_default() {
                        tasks.other.spawn(self.clone().establish_dht_connection(addr));
                    }
                }
            }
        }
    }

    // Periodically search for peers for the given repository and announce it on the DHT.
    // TODO: use some unique id instead of name.
    async fn find_peers_for_repository(&self, name: &str, index: &Index) {
        let dht = if let Some(dht) = &self.dht {
            dht.clone()
        } else {
            return;
        };

        let info_hash = repository_info_hash(name);
        let closed = index.subscribe().closed();

        let tasks_arc = self.tasks.upgrade().unwrap();
        let tasks = tasks_arc.write().await;

        tasks.other.spawn(async move {
            let search = async {
                loop {
                    // find peers for the repo and also announce that we have it.
                    dht.search(info_hash, true);

                    // sleep a random duration before the next search
                    let duration = rand::thread_rng()
                        .gen_range(MIN_DHT_ANNOUNCE_DELAY..MAX_DHT_ANNOUNCE_DELAY);
                    time::sleep(duration).await;
                }
            };

            // periodically re-announce the info-hash until the repository is closed.
            select! {
                _ = search => (),
                _ = closed => (),
            }
        })
    }

    async fn establish_user_provided_connection(self: Arc<Self>, addr: SocketAddr) {
        let tasks_arc = self.tasks.upgrade().unwrap();
        let tasks = tasks_arc.write().await;

        tasks.other.spawn(async move {
            loop {
                let permit = if let Some(permit) = self
                    .connection_deduplicator
                    .reserve(addr, ConnectionDirection::Outgoing)
                {
                    permit
                } else {
                    return;
                };

                let socket = self.keep_connecting(addr).await;

                self.clone()
                    .handle_new_connection(socket, PeerSource::UserProvided, permit)
                    .await;
            }
        })
    }

    async fn establish_discovered_connection(self: Arc<Self>, addr: SocketAddr) {
        let permit = if let Some(permit) = self
            .connection_deduplicator
            .reserve(addr, ConnectionDirection::Outgoing)
        {
            permit
        } else {
            return;
        };

        let socket = match TcpStream::connect(addr).await {
            Ok(socket) => socket,
            Err(error) => {
                log::error!("Failed to create outgoing TCP connection: {}", error);
                return;
            }
        };

        self.handle_new_connection(socket, PeerSource::LocalDiscovery, permit)
            .await;
    }

    async fn establish_dht_connection(self: Arc<Self>, addr: SocketAddr) {
        let permit = if let Some(permit) = self
            .connection_deduplicator
            .reserve(addr, ConnectionDirection::Outgoing)
        {
            permit
        } else {
            return;
        };

        // TODO: we should give up after a timeout
        let socket = self.keep_connecting(addr).await;

        self.handle_new_connection(socket, PeerSource::Dht, permit)
            .await;
    }

    async fn keep_connecting(&self, addr: SocketAddr) -> TcpStream {
        let mut i = 0;

        loop {
            match TcpStream::connect(addr).await {
                Ok(socket) => {
                    return socket;
                }
                Err(error) => {
                    // TODO: Might be worth randomizing this somehow.
                    let sleep_duration = Duration::from_secs(5)
                        .min(Duration::from_millis(200 * 2u64.pow(i.min(10))));
                    log::debug!(
                        "Failed to create outgoing TCP connection to {}: {}. Retrying in {:?}",
                        addr,
                        error,
                        sleep_duration
                    );
                    time::sleep(sleep_duration).await;
                    i = i.saturating_add(1);
                }
            }
        }
    }

    async fn handle_new_connection(
        self: Arc<Self>,
        socket: TcpStream,
        peer_source: PeerSource,
        permit: ConnectionPermit,
    ) {
        let addr = permit.addr();

        log::info!("New {} TCP connection: {}", peer_source, addr);

        let mut stream = TcpObjectStream::new(socket);
        let their_replica_id = match perform_handshake(&mut stream, &self.this_replica_id).await {
            Ok(replica_id) => replica_id,
            Err(error) => {
                log::error!("Failed to perform handshake: {}", error);
                return;
            }
        };

        // prevent self-connections.
        if their_replica_id == self.this_replica_id {
            log::debug!("Connection from self, discarding");
            return;
        }

        let released = permit.released();

        let mut brokers = self.message_brokers.lock().await;

        match brokers.entry(their_replica_id) {
            Entry::Occupied(entry) => entry.get().add_connection(stream, permit).await,
            Entry::Vacant(entry) => {
                log::info!("Connected to replica {:?}", their_replica_id);

                let broker = MessageBroker::new(their_replica_id, stream, permit).await;

                // TODO: for DHT connection we should only link the repository for which we did the
                // lookup but make sure we correctly handle edge cases, for example, when we have
                // more than one repository shared with the peer.
                for (id, holder) in &self.indices.read().await.map {
                    create_link(&broker, *id, holder.name.clone(), holder.index.clone()).await;
                }

                entry.insert(broker);
            }
        }

        drop(brokers);

        released.notified().await;
        log::info!("Lost {} TCP connection: {}", peer_source, addr);

        // Remove the broker if it has no more connections.
        let mut brokers = self.message_brokers.lock().await;
        if let Entry::Occupied(entry) = brokers.entry(their_replica_id) {
            if !entry.get().has_connections() {
                entry.remove();
            }
        }
    }
}

#[derive(Default)]
struct IndexMap {
    map: HashMap<RepositoryId, IndexHolder>,
    next_id: RepositoryId,
}

impl IndexMap {
    fn insert(&mut self, name: String, index: Index) -> Option<RepositoryId> {
        let id = self.next_id;

        match self.map.entry(id) {
            Entry::Vacant(entry) => {
                entry.insert(IndexHolder { index, name });
                self.next_id = self.next_id.wrapping_add(1);
                Some(id)
            }
            Entry::Occupied(_) => None,
        }
    }

    fn remove(&mut self, id: RepositoryId) {
        self.map.remove(&id);
    }
}

struct IndexHolder {
    index: Index,
    name: String,
}

async fn perform_handshake(
    stream: &mut TcpObjectStream,
    this_replica_id: &ReplicaId,
) -> io::Result<ReplicaId> {
    stream.write(this_replica_id).await?;
    stream.read().await
}

async fn create_link(broker: &MessageBroker, id: RepositoryId, name: String, index: Index) {
    // TODO: creating implicit link if the local and remote repository names are the same.
    // Eventually the links will be explicit.

    let local_id = Local::new(id);
    let local_name = Local::new(name.clone());
    let remote_name = Remote::new(name);

    broker
        .create_link(index, local_id, local_name, remote_name)
        .await
}

async fn dht_router_addresses() -> Vec<SocketAddr> {
    future::join_all(DHT_ROUTERS.iter().map(net::lookup_host))
        .await
        .into_iter()
        .filter_map(|result| result.ok())
        .flatten()
        .collect()
}

// Calculate info hash for a repository name.
// TODO: use some random unique id, not name.
fn repository_info_hash(name: &str) -> InfoHash {
    // Calculate the info hash by hashing the name with SHA-256 and taking the first 20 bytes.
    // (bittorrent uses SHA-1 but that is less secure).
    // `unwrap` is OK because the byte slice has the correct length.
    InfoHash::try_from(&name.as_bytes().hash().as_ref()[..INFO_HASH_LEN]).unwrap()
}
