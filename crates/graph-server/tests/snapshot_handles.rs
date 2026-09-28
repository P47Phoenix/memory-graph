//! Snapshot handles die with the connection that opened them (review item
//! 7): a client that vanishes does not pin snapshots until their TTL.
use graph_proto::pb;
use graph_proto::pb::store_client::StoreClient;
use graph_server::testing::TestServer;
use std::time::{Duration, Instant};

fn open_count(server: &TestServer) -> usize {
    let slot = &server.running().expect("running").slot;
    slot.with_store(|s| Ok(graph_store::Store::snapshot_stats(s).open_count))
        .unwrap()
}

#[test]
fn dropping_the_channel_drops_its_snapshot_handles_promptly() {
    let d = tempfile::tempdir().unwrap();
    let server = TestServer::start(&d.path().join("g.redb"), vec![]);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let endpoint = format!("http://{}", server.endpoint());
    rt.block_on(async {
        let channel = tonic::transport::Endpoint::from_shared(endpoint)
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut client = StoreClient::new(channel);
        for _ in 0..3 {
            client
                .open_snapshot(pb::OpenSnapshotRequest {})
                .await
                .unwrap();
        }
        drop(client);
    });
    assert_eq!(server.running().unwrap().slot.snapshots().len(), 3);
    // The runtime owns the connection task; dropping it closes the socket.
    drop(rt);
    let deadline = Instant::now() + Duration::from_secs(10);
    while open_count(&server) != 0 {
        assert!(
            Instant::now() < deadline,
            "handles still open {} after the channel was dropped",
            open_count(&server)
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(server.running().unwrap().slot.snapshots().is_empty());
}
