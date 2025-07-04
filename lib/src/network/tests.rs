use super::{
    choke::Choker,
    client::Client,
    message::{Message, Request, Response},
    request_tracker::RequestTracker,
    server::Server,
};
use crate::{
    crypto::sign::{Keypair, PublicKey},
    db,
    event::{Event, EventSender, Payload},
    protocol::{
        test_utils::Snapshot, Block, BlockId, Bump, RepositoryId, RootNode, SingleBlockPresence,
    },
    repository::{monitor::RepositoryMonitor, Vault},
    store::{Changeset, SnapshotWriter},
    test_utils,
    version_vector::VersionVector,
};
use futures_util::{future, TryStreamExt};
use metrics::NoopRecorder;
use rand::prelude::*;
use state_monitor::StateMonitor;
use std::{fmt, pin::pin};
use tempfile::TempDir;
use test_strategy::proptest;
use tokio::{
    select,
    sync::{
        broadcast::{self, error::RecvError},
        mpsc,
    },
    time::{self, Duration},
};
use tracing::Instrument;

const TIMEOUT: Duration = Duration::from_secs(60);

// Test complete transfer of one snapshot from one replica to another
// Also test a new snapshot transfer is performed after every local branch
// change.
//
// NOTE: Reducing the number of cases otherwise this test is too slow.
// TODO: Make it faster and increase the cases.
#[proptest(async = "tokio", cases = 8)]
async fn transfer_snapshot_between_two_replicas(
    #[strategy(0usize..32)] leaf_count: usize,
    #[strategy(0usize..2)] changeset_count: usize,
    #[strategy(1usize..4)] changeset_size: usize,
    #[strategy(test_utils::rng_seed_strategy())] rng_seed: u64,
) {
    transfer_snapshot_between_two_replicas_case(
        leaf_count,
        changeset_count,
        changeset_size,
        rng_seed,
    )
    .await
}

async fn transfer_snapshot_between_two_replicas_case(
    leaf_count: usize,
    changeset_count: usize,
    changeset_size: usize,
    rng_seed: u64,
) {
    test_utils::init_log();

    assert!(changeset_size > 0);

    let mut rng = StdRng::seed_from_u64(rng_seed);

    let write_keys = Keypair::generate(&mut rng);
    let (_a_base_dir, a_vault, _, a_choker, a_id) = create_repository(&mut rng, &write_keys).await;
    let (_b_base_dir, b_vault, b_request_tracker, _, _) =
        create_repository(&mut rng, &write_keys).await;

    let snapshot = Snapshot::generate(&mut rng, leaf_count);
    save_snapshot(&a_vault, a_id, &write_keys, &snapshot).await;
    save_blocks(&a_vault, &snapshot).await;

    assert!(load_latest_root_node(&b_vault, &a_id).await.is_none());

    let mut server = create_server(a_vault.clone(), a_choker);
    let mut client = create_client(b_vault.clone(), &b_request_tracker);

    // Wait until replica B catches up to replica A, then have replica A perform a local change
    // and repeat.
    let drive = async {
        let mut remaining_changesets = changeset_count;

        loop {
            wait_until_snapshots_in_sync(&a_vault, a_id, &b_vault).await;

            if remaining_changesets > 0 {
                tracing::info!("create changeset");
                create_changeset(&mut rng, &a_vault, &a_id, &write_keys, changeset_size).await;
                remaining_changesets -= 1;
            } else {
                break;
            }
        }
    };

    simulate_connection_until(&mut server, &mut client, drive).await;

    // HACK: prevent "too many open files" error.
    a_vault.store().close().await.unwrap();
    b_vault.store().close().await.unwrap();
}

// NOTE: Reducing the number of cases otherwise this test is too slow.
// TODO: Make it faster and increase the cases.
#[proptest(async = "tokio", cases = 8)]
async fn transfer_blocks_between_two_replicas(
    #[strategy(1usize..32)] block_count: usize,
    #[strategy(test_utils::rng_seed_strategy())] rng_seed: u64,
) {
    transfer_blocks_between_two_replicas_case(block_count, rng_seed).await
}

async fn transfer_blocks_between_two_replicas_case(block_count: usize, rng_seed: u64) {
    let mut rng = StdRng::seed_from_u64(rng_seed);

    let write_keys = Keypair::generate(&mut rng);
    let (_a_base_dir, a_vault, _, a_choker, a_id) = create_repository(&mut rng, &write_keys).await;
    let (_b_base_dir, b_vault, b_request_tracker, _, b_id) =
        create_repository(&mut rng, &write_keys).await;

    // Initially both replicas have the whole snapshot but no blocks.
    let snapshot = Snapshot::generate(&mut rng, block_count);
    save_snapshot(&a_vault, a_id, &write_keys, &snapshot).await;
    save_snapshot(&b_vault, b_id, &write_keys, &snapshot).await;

    let mut server = create_server(a_vault.clone(), a_choker.clone());
    let mut client = create_client(b_vault.clone(), &b_request_tracker);

    // Receive the blocks by replica A and verify they get received by replica B as well.
    let drive = async {
        // Simulate receiving the blocks from yet another replica
        let mut writer = a_vault.store().begin_client_write().await.unwrap();

        for (id, block) in snapshot.blocks() {
            writer.save_block(block).await.unwrap();
            tracing::info!(?id, "save block");
        }

        let status = writer.commit().await.unwrap();

        for block_id in status.new_blocks {
            a_vault.event_tx.send(Payload::BlockReceived(block_id));
        }

        // Then wait until replica B receives and writes it too.
        for id in snapshot.blocks().keys() {
            tracing::info!(?id, "waiting for block");
            wait_until_block_exists(&b_vault, id).await;
            tracing::info!(?id, "block received");
        }
    };

    tracing::info!("start");
    simulate_connection_until(&mut server, &mut client, drive).await;

    drop(client);

    // HACK: prevent "too many open files" error.
    a_vault.store().close().await.unwrap();
    b_vault.store().close().await.unwrap();
}

// Receive a `LeafNode` with non-missing block, then drop the connection before the block itself is
// received, then re-establish the connection and make sure the block gets received then.
#[tokio::test]
async fn failed_block_only_peer() {
    test_utils::init_log();

    let mut rng = StdRng::seed_from_u64(0);

    let write_keys = Keypair::generate(&mut rng);
    let (_a_base_dir, a_vault, _, a_choker, a_id) = create_repository(&mut rng, &write_keys).await;
    let (_b_base_dir, b_vault, b_request_tracker, _, _) =
        create_repository(&mut rng, &write_keys).await;

    let snapshot = Snapshot::generate(&mut rng, 1);
    save_snapshot(&a_vault, a_id, &write_keys, &snapshot).await;
    save_blocks(&a_vault, &snapshot).await;

    let mut server = create_server(a_vault.clone(), a_choker.clone());
    let mut client = create_client(b_vault.clone(), &b_request_tracker);

    simulate_connection_until(
        &mut server,
        &mut client,
        wait_until_snapshots_in_sync(&a_vault, a_id, &b_vault),
    )
    .await;

    // Simulate peer disconnecting and reconnecting.
    drop(server);
    drop(client);

    let mut server = create_server(a_vault.clone(), a_choker);
    let mut client = create_client(b_vault.clone(), &b_request_tracker);

    simulate_connection_until(&mut server, &mut client, async {
        for id in snapshot.blocks().keys() {
            wait_until_block_exists(&b_vault, id).await
        }
    })
    .await;
}

// Same as `failed_block_only_peer` test but this time there is a second peer who remains connected
// for the whole duration of the test. This is to uncover any potential request caching issues.
#[tokio::test]
async fn failed_block_same_peer() {
    test_utils::init_log();

    let mut rng = StdRng::seed_from_u64(0);

    let write_keys = Keypair::generate(&mut rng);
    let (_a_base_dir, a_vault, _, a_choker, a_id) = create_repository(&mut rng, &write_keys).await;
    let (_b_base_dir, b_vault, _, b_choker, _) = create_repository(&mut rng, &write_keys).await;
    let (_c_base_dir, c_vault, c_request_tracker, _, _) =
        create_repository(&mut rng, &write_keys).await;

    let snapshot = Snapshot::generate(&mut rng, 1);
    save_snapshot(&a_vault, a_id, &write_keys, &snapshot).await;
    save_blocks(&a_vault, &snapshot).await;

    // [A]-(server_ac)---+
    //                   |
    //               (client_ca)
    //                   |
    //                  [C]
    //                   |
    //               (client_cb)
    //                   |
    // [B]-(server_bc)---+

    let mut server_ac = create_server(a_vault.clone(), a_choker.clone());
    let mut client_ca = create_client(c_vault.clone(), &c_request_tracker);

    let mut server_bc = create_server(b_vault.clone(), b_choker);
    let mut client_cb = create_client(c_vault.clone(), &c_request_tracker);

    // Run both connections in parallel until C syncs its index (but not blocks) with A
    let conn_ac = simulate_connection(&mut server_ac, &mut client_ca);
    let conn_ac = conn_ac.instrument(tracing::info_span!("AC1"));

    let conn_bc = simulate_connection(&mut server_bc, &mut client_cb);
    let mut conn_bc = pin!(conn_bc.instrument(tracing::info_span!("BC")));

    run_until(
        future::join(conn_ac, &mut conn_bc),
        wait_until_snapshots_in_sync(&a_vault, a_id, &c_vault),
    )
    .await;

    // Drop and recreate the A-C connection but keep the B-C connection up.
    drop(server_ac);
    drop(client_ca);

    let mut server_ac = create_server(a_vault.clone(), a_choker);
    let mut client_ca = create_client(c_vault.clone(), &c_request_tracker);

    // Run the new A-C connection in parallel with the existing B-C connection until all blocks are
    // received.
    let conn_ac = simulate_connection(&mut server_ac, &mut client_ca);
    let conn_ac = conn_ac.instrument(tracing::info_span!("AC2"));

    run_until(future::join(conn_ac, conn_bc), async {
        for id in snapshot.blocks().keys() {
            wait_until_block_exists(&c_vault, id).await
        }
    })
    .await;
}

// This test verifies that when there are two peers that have a particular block, even when one of
// them drops, we can still succeed in retrieving the block from the remaining peer.
#[tokio::test]
#[ignore = "request tracker is not cancel safe"]
async fn failed_block_other_peer() {
    test_utils::init_log();

    // This test has a delicate setup phase which might not always succeed (it's not
    // deterministic) so the setup might need to be repeated multiple times.
    'main: loop {
        let mut rng = StdRng::seed_from_u64(0);

        let write_keys = Keypair::generate(&mut rng);
        let (_a_base_dir, a_vault, _, a_choker, a_id) =
            create_repository(&mut rng, &write_keys).await;
        let (_b_base_dir, b_vault, b_request_tracker, b_choker, b_id) =
            create_repository(&mut rng, &write_keys).await;
        let (_c_base_dir, c_vault, c_request_tracker, _, _) =
            create_repository(&mut rng, &write_keys).await;

        // Create the snapshot by A
        let snapshot = Snapshot::generate(&mut rng, 1);
        save_snapshot(&a_vault, a_id, &write_keys, &snapshot).await;
        save_blocks(&a_vault, &snapshot).await;

        // Sync B with A
        let mut server_ab = create_server(a_vault.clone(), a_choker.clone());
        let mut client_ba = create_client(b_vault.clone(), &b_request_tracker);
        simulate_connection_until(&mut server_ab, &mut client_ba, async {
            for id in snapshot.blocks().keys() {
                wait_until_block_exists(&b_vault, id).await;
            }
        })
        .await;
        drop(server_ab);
        drop(client_ba);

        // [A]-(server_ac)---+
        //                   |
        //               (client_ca)
        //                   |
        //                  [C]
        //                   |
        //               (client_cb)
        //                   |
        // [B]-(server_bc)---+

        let span_ac = tracing::info_span!("AC");
        let span_bc = tracing::info_span!("BC");

        let enter = span_ac.enter();
        let mut server_ac = create_server(a_vault.clone(), a_choker);
        let mut client_ca = create_client(c_vault.clone(), &c_request_tracker);
        drop(enter);

        let enter = span_bc.enter();
        let mut server_bc = create_server(b_vault.clone(), b_choker);
        let mut client_cb = create_client(c_vault.clone(), &c_request_tracker);
        drop(enter);

        // Run the two connections in parallel until C syncs its index with both A and B.
        let conn_bc = simulate_connection(&mut server_bc, &mut client_cb);
        let conn_bc = conn_bc.instrument(span_bc);
        let mut conn_bc = Box::pin(conn_bc);

        let conn_ac = simulate_connection(&mut server_ac, &mut client_ca);
        let conn_ac = conn_ac.instrument(span_ac.clone());

        run_until(future::join(conn_ac, &mut conn_bc), async {
            wait_until_snapshots_in_sync(&a_vault, a_id, &c_vault).await;
            wait_until_snapshots_in_sync(&b_vault, b_id, &c_vault).await;
        })
        .await;

        // Drop the A-C connection so C can't receive any blocks from A anymore.
        let enter = span_ac.enter();
        tracing::info!("dropping connection");
        drop(server_ac);
        drop(client_ca);
        drop(enter);

        // It might sometimes happen that the block were already received in the previous step
        // In that case the situation this test is trying to exercise does not occur and we need
        // to try again.
        let mut reader = c_vault.store().acquire_read().await.unwrap();
        for id in snapshot.blocks().keys() {
            if reader.block_exists(id).await.unwrap() {
                tracing::warn!("test preconditions not met, trying again");

                drop(reader);
                drop(conn_bc);

                a_vault.store().close().await.unwrap();
                b_vault.store().close().await.unwrap();
                c_vault.store().close().await.unwrap();

                continue 'main;
            }
        }
        drop(reader);

        tracing::info!("start");

        // Continue running the B-C connection and verify C receives the missing blocks from B who is
        // the only remaining peer at this point.
        run_until(conn_bc, async {
            for id in snapshot.blocks().keys() {
                wait_until_block_exists(&c_vault, id).await;
            }
        })
        .await;

        break;
    }
}

async fn create_repository<R: Rng + CryptoRng>(
    rng: &mut R,
    write_keys: &Keypair,
) -> (TempDir, Vault, RequestTracker, Choker, PublicKey) {
    let (base_dir, db) = db::create_temp().await.unwrap();

    let writer_id = PublicKey::generate(rng);
    let repository_id = RepositoryId::from(write_keys.public_key());
    let event_tx = EventSender::new(128);
    let monitor = RepositoryMonitor::new(StateMonitor::make_root(), &NoopRecorder);
    let traffic_monitor = monitor.traffic.clone();
    let state = Vault::new(repository_id, event_tx, db, monitor);
    let request_tracker = RequestTracker::new(traffic_monitor);
    let choker = Choker::new();

    (base_dir, state, request_tracker, choker, writer_id)
}

// Enough capacity to prevent deadlocks.
// TODO: find the actual minimum necessary capacity.
const CAPACITY: usize = 256;

async fn save_snapshot(
    vault: &Vault,
    writer_id: PublicKey,
    write_keys: &Keypair,
    snapshot: &Snapshot,
) {
    // If the snapshot is empty then there is nothing else to save in addition to the initial root
    // node the index already has.
    if snapshot.leaf_count() == 0 {
        return;
    }

    let mut version_vector = VersionVector::new();
    version_vector.insert(writer_id, 2); // to force overwrite the initial root node

    let status = SnapshotWriter::begin(vault.store(), snapshot)
        .await
        .save_nodes(write_keys, writer_id, version_vector)
        .await
        .commit()
        .await;

    for branch_id in status.approved_branches {
        vault.event_tx.send(Payload::SnapshotApproved(branch_id));
    }
}

async fn save_blocks(vault: &Vault, snapshot: &Snapshot) {
    let status = SnapshotWriter::begin(vault.store(), snapshot)
        .await
        .save_blocks()
        .await
        .commit()
        .await;

    for block_id in status.new_blocks {
        vault.event_tx.send(Payload::BlockReceived(block_id));
    }
}

async fn wait_until_snapshots_in_sync(
    server_vault: &Vault,
    server_id: PublicKey,
    client_vault: &Vault,
) {
    let mut rx = client_vault.event_tx.subscribe();

    let server_root = load_latest_root_node(server_vault, &server_id).await;
    let server_root = if let Some(server_root) = server_root {
        server_root
    } else {
        return;
    };

    if server_root.proof.version_vector.is_empty() {
        return;
    }

    loop {
        if let Some(client_root) = load_latest_root_node(client_vault, &server_id).await {
            if client_root.summary.state.is_approved()
                && client_root.proof.hash == server_root.proof.hash
            {
                // client has now fully downloaded server's latest snapshot.
                assert_eq!(
                    client_root.proof.version_vector,
                    server_root.proof.version_vector
                );
                break;
            }
        }

        recv_any(&mut rx).await
    }
}

async fn wait_until_block_exists(vault: &Vault, block_id: &BlockId) {
    let mut rx = vault.event_tx.subscribe();

    while !vault
        .store()
        .acquire_read()
        .await
        .unwrap()
        .block_exists(block_id)
        .await
        .unwrap()
    {
        recv_any(&mut rx).await
    }
}

async fn recv_any(rx: &mut broadcast::Receiver<Event>) {
    match rx.recv().await {
        Ok(_) | Err(RecvError::Lagged(_)) => (),
        Err(RecvError::Closed) => panic!("event channel unexpectedly closed"),
    }
}

// Simulate a changeset, e.g. create a file, write to it and flush it.
async fn create_changeset(
    rng: &mut StdRng,
    vault: &Vault,
    writer_id: &PublicKey,
    write_keys: &Keypair,
    size: usize,
) {
    assert!(size > 0);

    let mut tx = vault.store().begin_write().await.unwrap();
    let mut changeset = Changeset::new();

    for _ in 0..size {
        let encoded_locator = rng.r#gen();
        let block: Block = rng.r#gen();

        changeset.link_block(encoded_locator, block.id, SingleBlockPresence::Present);
        changeset.write_block(block);
    }

    changeset.bump(Bump::increment(*writer_id));
    changeset
        .apply(&mut tx, writer_id, write_keys)
        .await
        .unwrap();

    tx.commit().await.unwrap();

    vault.event_tx.send(Payload::SnapshotApproved(*writer_id));
}

async fn load_latest_root_node(vault: &Vault, writer_id: &PublicKey) -> Option<RootNode> {
    vault
        .store()
        .acquire_read()
        .await
        .unwrap()
        .load_root_nodes_by_writer(writer_id)
        .try_next()
        .await
        .unwrap()
}

// Simulate connection between two replicas until the given future completes.
async fn simulate_connection_until<F>(server: &mut ServerData, client: &mut ClientData, until: F)
where
    F: Future,
{
    run_until(simulate_connection(server, client), until).await
}

// Simulate connection forever.
async fn simulate_connection(server: &mut ServerData, client: &mut ClientData) {
    let (server, server_send_rx, server_recv_tx) = server;
    let (client, client_send_rx, client_recv_tx) = client;

    let mut server_conn = Connection {
        send_rx: server_send_rx,
        recv_tx: client_recv_tx,
    };

    let mut client_conn = Connection {
        send_rx: client_send_rx,
        recv_tx: server_recv_tx,
    };

    let server_run = server.run().instrument(tracing::info_span!("server"));
    let client_run = client.run().instrument(tracing::info_span!("client"));

    select! {
        biased; // deterministic poll order for repeatable tests

        result = server_run => result.unwrap(),
        result = client_run => result.unwrap(),
        _ = server_conn.run() => panic!("connection closed prematurely"),
        _ = client_conn.run() => panic!("connection closed prematurely"),
    }
}

// Runs `task` until `until` completes. Panics if `until` doesn't complete before `TIMEOUT` or if
// `task` completes before `until`.
async fn run_until<F, U>(task: F, until: U)
where
    F: Future,
    U: Future,
{
    select! {
        biased; // deterministic poll order for repeatable tests
        _ = task => panic!("task completed prematurely"),
        _ = until => (),
        _ = time::sleep(TIMEOUT) => panic!("test timed out"),
    }
}

type ServerData = (Server, mpsc::Receiver<Message>, mpsc::Sender<Request>);
type ClientData = (Client, mpsc::Receiver<Message>, mpsc::Sender<Response>);

fn create_server(vault: Vault, choker: Choker) -> ServerData {
    let (send_tx, send_rx) = mpsc::channel(1);
    let (recv_tx, recv_rx) = mpsc::channel(CAPACITY);
    let server = Server::new(vault, send_tx, recv_rx, choker);

    (server, send_rx, recv_tx)
}

fn create_client(vault: Vault, request_tracker: &RequestTracker) -> ClientData {
    let (send_tx, send_rx) = mpsc::channel(1);
    let (recv_tx, recv_rx) = mpsc::channel(CAPACITY);
    let client = Client::new(vault, send_tx, recv_rx, request_tracker);

    (client, send_rx, recv_tx)
}

// Simulated connection between a server and a client.
struct Connection<'a, T> {
    send_rx: &'a mut mpsc::Receiver<Message>,
    recv_tx: &'a mut mpsc::Sender<T>,
}

impl<T> Connection<'_, T>
where
    T: From<Message> + fmt::Debug,
{
    async fn run(&mut self) {
        while let Some(content) = self.send_rx.recv().await {
            self.recv_tx.send(content.into()).await.unwrap();
        }
    }
}
