use fusion_supervisor::probe::{probe, ProbeResult, ProbeSpec, UnhealthyReason};

#[tokio::test]
async fn test_probe_http_healthy() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        loop {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let resp = "HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n";
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let r = probe(&ProbeSpec::http(port, "/healthz")).await;
    assert!(matches!(r, ProbeResult::Healthy { .. }));
}

#[tokio::test]
async fn test_probe_http_conn_refused() {
    let r = probe(&ProbeSpec::http(59999, "/health")).await;
    assert!(matches!(r, ProbeResult::Unhealthy { reason: UnhealthyReason::ConnectionRefused }));
}

#[tokio::test]
async fn test_probe_http_404_badpath() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        loop {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let resp = "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n";
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let r = probe(&ProbeSpec::http(port, "/healthz")).await;
    assert!(matches!(r, ProbeResult::Unhealthy { reason: UnhealthyReason::BadPath(404) }));
}
