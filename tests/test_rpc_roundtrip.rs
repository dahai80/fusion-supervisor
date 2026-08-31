use fusion_supervisor::manifest::{Layer, ServiceDef};
use fusion_supervisor::rpc::client::RpcClient;
use fusion_supervisor::supervisor::SupervisorCore;
use std::sync::Arc;

#[tokio::test]
async fn test_rpc_ping_roundtrip() {
    let defs = vec![ServiceDef {
        name: "fusion-mlx".into(),
        port: 11434,
        repo_dir: "fusion-mlx".into(),
        purpose: "x".into(),
        fixed: true,
        layer: Layer::Core,
    }];
    let db_path = std::env::temp_dir().join(format!("fusion-sv-rpc-{}.db", std::process::id()));
    let cfg = fusion_supervisor::config::Config {
        db_path: db_path.clone(),
        ..fusion_supervisor::config::Config::default()
    };
    let core = SupervisorCore::new(defs, cfg.clone()).unwrap();
    let core = Arc::new(tokio::sync::Mutex::new(core));
    let sock = format!("/tmp/fusion-sv-rpc-{}.sock", std::process::id());
    let _ = std::fs::remove_file(&sock);
    let sock_clone = sock.clone();
    let server = tokio::spawn(async move {
        let _ = fusion_supervisor::rpc::server::serve(core, &sock_clone, None).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let mut client = RpcClient::connect(&sock).await.unwrap();
    let ok = client.ping().await.unwrap();
    assert!(ok);
    server.abort();
    let _ = std::fs::remove_file(&sock);
    cleanup_db(&cfg.db_path);
}

fn cleanup_db(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let parent = path.parent().unwrap_or(std::path::Path::new("."));
    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
        let _ = std::fs::remove_file(parent.join(format!("{stem}.db-wal")));
        let _ = std::fs::remove_file(parent.join(format!("{stem}.db-shm")));
    }
}
