use super::{ip_stack::IpStack, socket};
use crate::{
    config::{ConfigKey, ConfigStore},
    scoped_task::{self, ScopedJoinHandle},
    state_monitor::StateMonitor,
};
use btdht::{InfoHash, MainlineDht};
use chrono::{offset::Local, DateTime};
use futures_util::{future, stream, StreamExt};
use rand::Rng;
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    future::pending,
    io,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, RwLock, Weak,
    },
    time::{Duration, SystemTime},
};
use tokio::{
    net::{self, UdpSocket},
    select,
    sync::{mpsc, watch},
    time,
};

// Hardcoded DHT routers to bootstrap the DHT against.
// TODO: add this to `NetworkOptions` so it can be overriden by the user.
pub const DHT_ROUTERS: &[&str] = &["router.bittorrent.com:6881", "dht.transmissionbt.com:6881"];

// Interval for the delay before a repository is re-announced on the DHT. The actual delay is an
// uniformly random value from this interval.
// BEP5 indicates that "After 15 minutes of inactivity, a node becomes questionable." so try not
// to get too close to that value to avoid DHT churn. However, too frequent updates may cause
// other nodes to put us on a blacklist.
pub const MIN_DHT_ANNOUNCE_DELAY: Duration = Duration::from_secs(3 * 60);
pub const MAX_DHT_ANNOUNCE_DELAY: Duration = Duration::from_secs(6 * 60);

const LAST_USED_PORT_V4: ConfigKey<u16> =
    ConfigKey::new("last_used_udp_port_v4", LAST_USED_PORT_COMMENT);

const LAST_USED_PORT_V6: ConfigKey<u16> =
    ConfigKey::new("last_used_udp_port_v6", LAST_USED_PORT_COMMENT);

const LAST_USED_PORT_COMMENT: &str =
    "The value stored in this file is the last used UDP port for listening on incoming\n\
     connections. It is used to avoid binding to a random port every time the application starts.\n\
     This, in turn, is mainly useful for users who can't or don't want to use UPnP and have to\n\
     default to manually setting up port forwarding on their routers.";

pub(super) struct DhtDiscovery {
    dhts: IpStack<MonitoredDht>,
    lookups: Arc<Mutex<Lookups>>,
    next_id: AtomicU64,
    monitor: Arc<StateMonitor>,
}

impl DhtDiscovery {
    pub async fn new(
        sockets: IpStack<UdpSocket>,
        acceptor_port: u16,
        monitor: Arc<StateMonitor>,
    ) -> Self {
        let (routers_v4, routers_v6) = dht_router_addresses().await;
        let lookups = Arc::new(Mutex::new(HashMap::default()));

        let dhts = match sockets {
            IpStack::V4(socket) => {
                IpStack::V4(start_dht(socket, acceptor_port, routers_v4, &monitor))
            }
            IpStack::V6(socket) => {
                IpStack::V6(start_dht(socket, acceptor_port, routers_v6, &monitor))
            }
            IpStack::Dual { v4, v6 } => IpStack::Dual {
                v4: start_dht(v4, acceptor_port, routers_v4, &monitor),
                v6: start_dht(v6, acceptor_port, routers_v6, &monitor),
            },
        };

        Self {
            dhts,
            lookups,
            next_id: AtomicU64::new(0),
            monitor,
        }
    }

    pub fn lookup(
        &self,
        info_hash: InfoHash,
        found_peers_tx: mpsc::UnboundedSender<SocketAddr>,
    ) -> LookupRequest {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);

        let request = LookupRequest {
            id,
            info_hash,
            lookups: Arc::downgrade(&self.lookups),
        };

        self.lookups
            .lock()
            .unwrap()
            .entry(info_hash)
            .or_insert_with(|| {
                Lookup::new(
                    self.dhts.as_ref().map(|e| e.dht.clone()),
                    info_hash,
                    &self.monitor,
                )
            })
            .add_request(id, found_peers_tx);

        request
    }
}

fn start_dht(
    socket: UdpSocket,
    acceptor_port: u16,
    routers: Vec<SocketAddr>,
    monitor: &Arc<StateMonitor>,
) -> MonitoredDht {
    let protocol = match socket.local_addr() {
        Ok(SocketAddr::V4(_)) => "IPv4",
        Ok(SocketAddr::V6(_)) => "IPv6",
        Err(_) => "?",
    };

    let monitor = monitor.make_child(protocol);

    // TODO: load the DHT state from a previous save if it exists.
    let dht = MainlineDht::builder()
        .add_routers(routers)
        .set_read_only(false)
        .set_announce_port(acceptor_port)
        .start(socket);

    // Spawn a task to monitor the DHT status.
    let monitoring_task = scoped_task::spawn({
        let dht = dht.clone();

        let bootstrap_state =
            monitor.make_value::<&'static str>("bootstrap_state".into(), "bootstrapping");

        async move {
            if dht.bootstrapped().await {
                *bootstrap_state.get() = "bootstrapped";
                log::info!("DHT {} bootstrap complete", protocol);
            } else {
                *bootstrap_state.get() = "bootstrap failed";
                log::error!("DHT {} bootstrap failed", protocol);

                // Don't `return`, instead halt here so that the `bootstrap_state` monitored value
                // is preserved for the user to see.
                pending::<()>().await;
            }

            let i = monitor.make_value::<u64>("probe_counter".into(), 0);

            let is_running = monitor.make_value::<Option<bool>>("is_running".into(), None);
            let good_node_count =
                monitor.make_value::<Option<usize>>("good_node_count".into(), None);
            let questionable_node_count =
                monitor.make_value::<Option<usize>>("questionable_node_count".into(), None);
            let bucket_count = monitor.make_value::<Option<usize>>("bucket_count".into(), None);

            loop {
                *i.get() += 1;

                if let Some(state) = dht.get_debug_state().await {
                    *is_running.get() = Some(state.is_running);
                    *good_node_count.get() = Some(state.good_node_count);
                    *questionable_node_count.get() = Some(state.questionable_node_count);
                    *bucket_count.get() = Some(state.bucket_count);
                } else {
                    *is_running.get() = None;
                    *good_node_count.get() = None;
                    *questionable_node_count.get() = None;
                    *bucket_count.get() = None;
                }
                time::sleep(Duration::from_secs(5)).await;
            }
        }
    });

    MonitoredDht {
        dht,
        _monitoring_task: monitoring_task,
    }
}

struct MonitoredDht {
    dht: MainlineDht,
    _monitoring_task: ScopedJoinHandle<()>,
}

type Lookups = HashMap<InfoHash, Lookup>;

type RequestId = u64;

pub struct LookupRequest {
    id: RequestId,
    info_hash: InfoHash,
    lookups: Weak<Mutex<Lookups>>,
}

impl Drop for LookupRequest {
    fn drop(&mut self) {
        if let Some(lookups) = self.lookups.upgrade() {
            let mut lookups = lookups.lock().unwrap();

            let empty = if let Some(lookup) = lookups.get_mut(&self.info_hash) {
                let mut requests = lookup.requests.lock().unwrap();
                requests.remove(&self.id);
                requests.is_empty()
            } else {
                false
            };

            if empty {
                lookups.remove(&self.info_hash);
            }
        }
    }
}

struct Lookup {
    seen_peers: Arc<RwLock<HashSet<SocketAddr>>>, // To avoid duplicates
    requests: Arc<Mutex<HashMap<RequestId, mpsc::UnboundedSender<SocketAddr>>>>,
    wake_up_tx: watch::Sender<()>,
    _task: ScopedJoinHandle<()>,
}

impl Lookup {
    fn new(dhts: IpStack<MainlineDht>, info_hash: InfoHash, monitor: &Arc<StateMonitor>) -> Self {
        let (wake_up_tx, mut wake_up_rx) = watch::channel(());
        // Mark the initial value as seen so the change notification is not triggered immediately
        // but only when we create the first request.
        wake_up_rx.borrow_and_update();

        let monitor = monitor
            .make_child("lookups")
            .make_child(format!("{:?}", info_hash));

        let seen_peers = Arc::new(RwLock::new(Default::default()));
        let requests = Arc::new(Mutex::new(HashMap::new()));

        let task = Self::start_task(
            dhts,
            info_hash,
            seen_peers.clone(),
            requests.clone(),
            wake_up_rx,
            &monitor,
        );

        Lookup {
            seen_peers,
            requests,
            wake_up_tx,
            _task: task,
        }
    }

    fn add_request(&mut self, id: RequestId, tx: mpsc::UnboundedSender<SocketAddr>) {
        for peer in self.seen_peers.read().unwrap().iter() {
            tx.send(*peer).unwrap_or(());
        }

        self.requests.lock().unwrap().insert(id, tx);
        self.wake_up_tx.send(()).unwrap();
    }

    fn start_task(
        dhts: IpStack<MainlineDht>,
        info_hash: InfoHash,
        seen_peers: Arc<RwLock<HashSet<SocketAddr>>>,
        requests: Arc<Mutex<HashMap<RequestId, mpsc::UnboundedSender<SocketAddr>>>>,
        mut wake_up: watch::Receiver<()>,
        monitor: &Arc<StateMonitor>,
    ) -> ScopedJoinHandle<()> {
        let state =
            monitor.make_value::<Cow<'static, str>>("state".into(), Cow::Borrowed("started"));

        scoped_task::spawn(async move {
            // Wait for the first request to be created
            wake_up.changed().await.unwrap_or(());

            loop {
                log::debug!("DHT Starting search for info hash: {:?}", info_hash);
                *state.get() = Cow::Borrowed("making request");

                // find peers for the repo and also announce that we have it.
                let mut peers =
                    stream::iter(dhts.iter()).flat_map(|dht| dht.search(info_hash, true));

                *state.get() = Cow::Borrowed("awaiting results");
                while let Some(peer) = peers.next().await {
                    if seen_peers.write().unwrap().insert(peer) {
                        log::debug!("DHT found peer for {:?}: {}", info_hash, peer);

                        for tx in requests.lock().unwrap().values() {
                            tx.send(peer).unwrap_or(());
                        }
                    }
                }

                seen_peers.write().unwrap().clear();

                // sleep a random duration before the next search, but wake up if there is a new
                // request.
                let duration =
                    rand::thread_rng().gen_range(MIN_DHT_ANNOUNCE_DELAY..MAX_DHT_ANNOUNCE_DELAY);

                {
                    let time: DateTime<Local> = (SystemTime::now() + duration).into();
                    log::debug!(
                        "DHT search for {:?} ended. Next one scheduled at {} (in {:?})",
                        info_hash,
                        time.format("%T"),
                        duration
                    );
                    *state.get() = Cow::Owned(format!("sleeping until {}", time.format("%T")));
                }

                select! {
                    _ = time::sleep(duration) => {
                        log::debug!("DHT sleep duration passed")
                    },
                    _ = wake_up.changed() => {
                        log::debug!("DHT wake up")
                    },
                }
            }
        })
    }
}

// Returns the router addresses split into ipv4 and ipv6 (in that order)
pub async fn dht_router_addresses() -> (Vec<SocketAddr>, Vec<SocketAddr>) {
    future::join_all(DHT_ROUTERS.iter().map(net::lookup_host))
        .await
        .into_iter()
        .filter_map(|result| result.ok())
        .flatten()
        .partition(|addr| addr.is_ipv4())
}

pub(super) async fn bind(config: &ConfigStore) -> io::Result<IpStack<UdpSocket>> {
    // TODO: [BEP-32](https://www.bittorrent.org/beps/bep_0032.html) says we should bind the ipv6
    // socket to a concrete unicast address, not to an unspecified one. Consider finding somehow
    // what the local ipv6 address of this device is and use that instead.
    match (
        socket::bind(
            (Ipv4Addr::UNSPECIFIED, 0).into(),
            config.entry(LAST_USED_PORT_V4),
        )
        .await,
        socket::bind(
            (Ipv6Addr::UNSPECIFIED, 0).into(),
            config.entry(LAST_USED_PORT_V6),
        )
        .await,
    ) {
        (Ok(v4), Ok(v6)) => Ok(IpStack::Dual { v4, v6 }),
        (Ok(v4), Err(error)) => {
            log::warn!(
                "failed to bind DHT to ipv6 address: {}, using ipv4 only",
                error
            );
            Ok(IpStack::V4(v4))
        }
        (Err(error), Ok(v6)) => {
            log::warn!(
                "failed to bind DHT to ipv4 address: {}, using ipv6 only",
                error
            );
            Ok(IpStack::V6(v6))
        }
        (Err(error_v4), Err(error_v6)) => {
            log::error!("failed to bind DHT to ipv4 address: {}", error_v4);
            log::error!("failed to bind DHT to ipv6 address: {}", error_v6);
            Err(error_v4) // we can return only one error - arbitrarily returning the v4 one.
        }
    }
}
