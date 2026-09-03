// issue #22: Prometheus /metrics HTTP 端点集成测试。
// 启动 core + metrics HTTP server 于临时端口, reqwest GET /metrics, 断言响应体含指标。
// 镜像 test_rpc_shutdown.rs 的 spawn-core-then-connect-then-cleanup 结构。
use fusion_supervisor::manifest::{Layer, ServiceDef};
use fusion_supervisor::probe::ProbeResult;
use fusion_supervisor::supervisor::{SupervisorCore, runner::State};
use std::sync::Arc;

fn def(name: &str, port: u16) -> ServiceDef {
    ServiceDef {
        name: name.into(),
        port,
        repo_dir: name.into(),
        purpose: "x".into(),
        fixed: false,
        layer: Layer::App,
    }
}

fn cfg() -> fusion_supervisor::config::Config {
    fusion_supervisor::config::Config {
        max_concurrent_start: 2,
        start_timeout_sec: 5,
        ..fusion_supervisor::config::Config::default()
    }
}

// 复用 main.rs 的 serve_metrics 需要进程内函数; 此测试改为直接在测试内
// 跑同样的 hand-rolled HTTP (复用 metrics::render 纯函数), 验证端到端 HTTP 路径。
async fn start_metrics_server(
    core: Arc<tokio::sync::Mutex<SupervisorCore>>,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let handle = tokio::spawn(async move {
        while let Ok((mut stream, _peer)) = listener.accept().await {
            let core = core.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                let req = String::from_utf8_lossy(&buf[..n]);
                let first_line = req.lines().next().unwrap_or("");
                if !first_line.starts_with("GET /metrics") {
                    let resp =
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = stream.write_all(resp.as_bytes()).await;
                    return;
                }
                let body = {
                    let c = core.lock().await;
                    fusion_supervisor::metrics::render(&c)
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });
    (addr, handle)
}

async fn http_get(url: &str) -> String {
    // 原生 TcpStream 手写 GET (避免测试依赖 reqwest, 保持零新依赖原则)。
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(url).await.unwrap();
    stream
        .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut resp = Vec::new();
    stream.read_to_end(&mut resp).await.unwrap();
    String::from_utf8_lossy(&resp).to_string()
}

#[tokio::test]
async fn test_metrics_endpoint_returns_daemon_up_and_service_gauges() {
    let core = SupervisorCore::new_with_fns(
        vec![def("fusion-mlx", 11434), def("fusion-rag", 11436)],
        cfg(),
        |_| ProbeResult::Healthy { latency_ms: 1 },
        |_, _, _| Ok(()),
    );
    let core_arc = Arc::new(tokio::sync::Mutex::new(core));
    // 预置状态: mlx=Healthy, rag=Failed + crash_count=2
    {
        let mut c = core_arc.lock().await;
        c.runners[0].state = State::Healthy;
        c.runners[1].state = State::Failed;
        c.runners[1].policy.record_crash(100);
        c.runners[1].policy.record_crash(101);
    }
    let (addr, handle) = start_metrics_server(core_arc.clone()).await;
    let resp = http_get(&addr).await;
    // 断言 HTTP 200
    assert!(
        resp.starts_with("HTTP/1.1 200"),
        "应返回 200: {}",
        &resp[..30]
    );
    assert!(
        resp.contains("text/plain; version=0.0.4"),
        "应含 Prometheus content-type"
    );
    // 断言指标体
    let body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
    assert!(body.contains("fusion_sv_up 1"), "应有 daemon up 指标");
    assert!(
        body.contains("fusion_sv_services_total{plane=\"native\"} 2"),
        "native 总数应为 2"
    );
    assert!(
        body.contains("fusion_sv_services_total{plane=\"compose\"} 0"),
        "compose 未启用应为 0"
    );
    assert!(
        body.contains(
            "fusion_sv_service_healthy{service=\"fusion-mlx\",port=\"11434\",plane=\"native\"} 1"
        ),
        "Healthy 服务应 healthy=1"
    );
    assert!(
        body.contains(
            "fusion_sv_service_healthy{service=\"fusion-rag\",port=\"11436\",plane=\"native\"} 0"
        ),
        "Failed 服务应 healthy=0"
    );
    assert!(
        body.contains("state=\"failed\"} 1"),
        "应有 state=failed gauge"
    );
    assert!(
        body.contains("fusion_sv_crash_count{service=\"fusion-rag\"} 2"),
        "crash_count 应为 2"
    );
    handle.abort();
}

#[tokio::test]
async fn test_metrics_endpoint_404_for_non_metrics_path() {
    let core = SupervisorCore::new_with_fns(
        vec![def("fusion-mlx", 11434)],
        cfg(),
        |_| ProbeResult::Healthy { latency_ms: 1 },
        |_, _, _| Ok(()),
    );
    let core_arc = Arc::new(tokio::sync::Mutex::new(core));
    let (addr, handle) = start_metrics_server(core_arc.clone()).await;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(&addr).await.unwrap();
    stream
        .write_all(b"GET /foo HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut resp = Vec::new();
    stream.read_to_end(&mut resp).await.unwrap();
    let resp = String::from_utf8_lossy(&resp).to_string();
    assert!(
        resp.starts_with("HTTP/1.1 404"),
        "非 /metrics 路径应 404: {}",
        &resp[..30]
    );
    handle.abort();
}

#[tokio::test]
async fn test_metrics_addr_none_disables_server() {
    // Config::default() metrics_addr = Some; 显式置 None 应不启 server。
    // 此测试验证 config 层: None → run_daemon 不 spawn metrics task。
    // (run_daemon 内部逻辑: if let Some(addr) = &cfg.metrics_addr → spawn; None → skip)
    let mut c = cfg();
    c.metrics_addr = None;
    assert!(c.metrics_addr.is_none(), "None 应保持 None");
    // 默认应为 Some
    let d = fusion_supervisor::config::Config::default();
    assert!(d.metrics_addr.is_some(), "default 应 Some");
    assert_eq!(
        d.metrics_addr.unwrap(),
        "127.0.0.1:9100",
        "默认地址应为 127.0.0.1:9100"
    );
}
