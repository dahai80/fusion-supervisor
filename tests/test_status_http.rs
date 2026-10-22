// issue #26: 只读 HTTP 状态面 (GET /api/v1/status) 集成测试。
// 镜像 test_metrics.rs 的 spawn-core-then-connect-then-cleanup 结构:
// 测试内跑同样的 hand-rolled HTTP (复用 metrics::render_status_json 纯函数),
// 验证端到端 HTTP 路径: status JSON / metrics 仍通 / 未知路径 404。
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
    fusion_supervisor::config::Config::default()
}

// 与 main.rs handle_metrics_conn 同逻辑的测试内 HTTP 循环。
async fn start_status_server(
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
                let is_status = first_line.starts_with("GET /api/v1/status");
                let is_metrics = first_line.starts_with("GET /metrics");
                if !is_metrics && !is_status {
                    let resp =
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = stream.write_all(resp.as_bytes()).await;
                    return;
                }
                let (content_type, body) = if is_status {
                    let body = {
                        let c = core.lock().await;
                        fusion_supervisor::metrics::render_status_json(&c)
                    };
                    ("application/json; charset=utf-8", body)
                } else {
                    let body = {
                        let c = core.lock().await;
                        fusion_supervisor::metrics::render(&c)
                    };
                    ("text/plain; version=0.0.4; charset=utf-8", body)
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });
    (addr, handle)
}

async fn http_get(url: &str, path: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(url).await.unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut resp = Vec::new();
    stream.read_to_end(&mut resp).await.unwrap();
    String::from_utf8_lossy(&resp).to_string()
}

#[tokio::test]
async fn test_status_endpoint_mirrors_status_rpc_fields() {
    let core = SupervisorCore::new_with_fns(
        vec![def("fusion-mlx", 11434), def("fusion-agent-studio", 11442)],
        cfg(),
        |_| ProbeResult::Healthy { latency_ms: 1 },
        |_, _, _| Ok(()),
    );
    let core_arc = Arc::new(tokio::sync::Mutex::new(core));
    {
        let mut c = core_arc.lock().await;
        c.runners[0].state = State::Healthy;
        c.runners[1].state = State::Stopped;
    }
    let (addr, handle) = start_status_server(core_arc.clone()).await;
    let resp = http_get(&addr, "/api/v1/status").await;
    assert!(
        resp.starts_with("HTTP/1.1 200"),
        "应返回 200: {}",
        &resp[..30]
    );
    assert!(resp.contains("application/json"), "应含 JSON content-type");
    let body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
    let v: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(v["ok"], serde_json::json!(true));
    let svcs = v["services"].as_array().unwrap();
    assert_eq!(svcs.len(), 2);
    assert_eq!(svcs[0]["name"], "fusion-mlx");
    assert_eq!(svcs[0]["state"], "Healthy");
    assert_eq!(svcs[0]["port"], 11434);
    assert_eq!(svcs[0]["plane"], "native");
    assert_eq!(svcs[1]["name"], "fusion-agent-studio");
    assert_eq!(svcs[1]["state"], "Stopped");
    handle.abort();
}

#[tokio::test]
async fn test_status_server_metrics_still_works_and_unknown_404() {
    let core = SupervisorCore::new_with_fns(
        vec![def("fusion-mlx", 11434)],
        cfg(),
        |_| ProbeResult::Healthy { latency_ms: 1 },
        |_, _, _| Ok(()),
    );
    let core_arc = Arc::new(tokio::sync::Mutex::new(core));
    let (addr, handle) = start_status_server(core_arc.clone()).await;
    // /metrics 不受影响
    let m = http_get(&addr, "/metrics").await;
    assert!(m.starts_with("HTTP/1.1 200"), "metrics 应仍 200");
    assert!(m.contains("fusion_sv_up 1"));
    // 未知路径 404 (无新路由面)
    let nf = http_get(&addr, "/api/v1/up").await;
    assert!(
        nf.starts_with("HTTP/1.1 404"),
        "变更操作不应出现在 HTTP 面: {}",
        &nf[..30]
    );
    handle.abort();
}
