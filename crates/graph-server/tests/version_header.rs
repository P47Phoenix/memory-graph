//! The server refuses a call whose `mg-protocol-version` header names
//! another version (review item 22), on every service, not only `Hello`.
use graph_proto::pb;
use graph_proto::pb::store_client::StoreClient;
use graph_proto::{WireError, PROTOCOL_VERSION_HEADER};
use graph_server::testing::TestServer;
use tonic::Code;

#[test]
fn a_mismatched_version_header_is_refused_and_a_matching_one_passes() {
    let d = tempfile::tempdir().unwrap();
    let server = TestServer::start(&d.path().join("g.redb"), vec![]);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let endpoint = format!("http://{}", server.endpoint());
    rt.block_on(async {
        let ch = tonic::transport::Endpoint::from_shared(endpoint)
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut client = StoreClient::new(ch);
        let call = |v: Option<&str>| {
            let mut req = tonic::Request::new(pb::SnapshotStatsRequest {});
            if let Some(v) = v {
                req.metadata_mut()
                    .insert(PROTOCOL_VERSION_HEADER, v.parse().unwrap());
            }
            req
        };
        let st = client.snapshot_stats(call(Some("99"))).await.unwrap_err();
        assert_eq!(st.code(), Code::FailedPrecondition);
        assert!(
            matches!(WireError::from(&st), WireError::Protocol(ref m) if m.contains("version 99")),
            "{st:?}"
        );
        client.snapshot_stats(call(Some("1"))).await.unwrap();
        client.snapshot_stats(call(None)).await.unwrap();
    });
}
