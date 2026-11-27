use fusion_supervisor::probe::{ProbeResult, ProbeSpec, UnhealthyReason, probe};

#[tokio::test]
async fn test_probe_uds_healthy() {
    let sock = format!("/tmp/fusion-sv-test-{}.sock", std::process::id());
    let _ = std::fs::remove_file(&sock);
    let sock_clone = sock.clone();
    tokio::spawn(async move {
        let listener = tokio::net::UnixListener::bind(&sock_clone).unwrap();
        loop {
            if let Ok((mut s, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                // the real daemon is a newline-delimited protocol: read one ping line then reply
                let mut line = [0u8; 512];
                if s.read(&mut line).await.unwrap_or(0) == 0 {
                    continue;
                }
                let _ = s
                    .write_all(br#"{"jsonrpc":"2.0","result":"pong","id":1}"#)
                    .await;
            }
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let r = probe(&ProbeSpec::uds(&sock)).await;
    assert!(matches!(r, ProbeResult::Healthy { .. }));
    let _ = std::fs::remove_file(&sock);
}

#[tokio::test]
async fn test_probe_uds_conn_fail() {
    let r = probe(&ProbeSpec::uds("/tmp/fusion-sv-nonexistent.sock")).await;
    assert!(matches!(
        r,
        ProbeResult::Unhealthy {
            reason: UnhealthyReason::UdsConnectFailed
        }
    ));
}

// a dispatch-stuck daemon (accepts but never replies) must not get a fake Healthy
#[tokio::test]
async fn test_probe_uds_silent_server_is_bad_reply() {
    let sock = format!("/tmp/fusion-sv-silent-{}.sock", std::process::id());
    let _ = std::fs::remove_file(&sock);
    let sock_clone = sock.clone();
    tokio::spawn(async move {
        let listener = tokio::net::UnixListener::bind(&sock_clone).unwrap();
        loop {
            if let Ok((mut s, _)) = listener.accept().await {
                use tokio::io::AsyncReadExt;
                // read but never reply — simulating a stuck dispatch
                let mut line = Vec::new();
                let _ = s.read(&mut line).await;
                std::mem::forget(s);
            }
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let r = probe(&ProbeSpec::uds(&sock)).await;
    assert!(matches!(
        r,
        ProbeResult::Unhealthy {
            reason: UnhealthyReason::UdsBadReply
        }
    ));
    let _ = std::fs::remove_file(&sock);
}
