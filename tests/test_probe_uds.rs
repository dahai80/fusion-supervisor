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
                // 真 daemon 是 newline-delimited 协议: 先读一行 ping 再应答
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

// dispatch 卡死的 daemon (accept 但不应答) 不能得假 Healthy
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
                // 读但不回 — 模拟 dispatch 卡死
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
