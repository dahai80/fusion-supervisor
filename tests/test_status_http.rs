// issue #26/#25: HTTP 面集成测试。
// 直接 spawn lib 的 http_server::serve (真实现, 非测试内复制), 验证:
// status 镜像 / metrics 不受影响 / history+events / 编排操作 token 校验 /
// web 静态文件 + 路径穿越拒绝 / 未知路径 404。
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

// 真实现 server: 随机端口, 可选 token / web_dir。
async fn start_server(
    core: Arc<tokio::sync::Mutex<SupervisorCore>>,
    token: Option<String>,
    web_dir: Option<String>,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    drop(listener);
    let addr_for_task = addr.clone();
    let shutdown = Arc::new(tokio::sync::Mutex::new(None));
    let handle = tokio::spawn(async move {
        let _ =
            fusion_supervisor::http_server::serve(core, &addr_for_task, token, web_dir, shutdown)
                .await;
    });
    (addr, handle)
}

async fn http_req(url: &str, req: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // spawn 后 listener 可能未就绪 — 重试连接 (最多 ~1s)。
    let mut stream = None;
    for _ in 0..50 {
        match tokio::net::TcpStream::connect(url).await {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
        }
    }
    let mut stream = stream.expect("server 未就绪 (1s 内连接失败)");
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut resp = Vec::new();
    stream.read_to_end(&mut resp).await.unwrap();
    String::from_utf8_lossy(&resp).to_string()
}

async fn http_get(url: &str, path: &str) -> String {
    http_req(
        url,
        &format!("GET {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n"),
    )
    .await
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
    let (addr, handle) = start_server(core_arc.clone(), None, None).await;
    let resp = http_get(&addr, "/api/v1/status").await;
    assert!(resp.starts_with("HTTP/1.1 200"), "应 200: {}", &resp[..30]);
    assert!(resp.contains("application/json"));
    let body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
    let v: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(v["ok"], serde_json::json!(true));
    let svcs = v["services"].as_array().unwrap();
    assert_eq!(svcs.len(), 2);
    assert_eq!(svcs[0]["name"], "fusion-mlx");
    assert_eq!(svcs[0]["state"], "Healthy");
    assert_eq!(svcs[0]["port"], 11434);
    assert_eq!(svcs[0]["plane"], "native");
    assert_eq!(svcs[1]["state"], "Stopped");
    handle.abort();
}

#[tokio::test]
async fn test_metrics_health_and_unknown_404() {
    let core = SupervisorCore::new_with_fns(
        vec![def("fusion-mlx", 11434)],
        cfg(),
        |_| ProbeResult::Healthy { latency_ms: 1 },
        |_, _, _| Ok(()),
    );
    let core_arc = Arc::new(tokio::sync::Mutex::new(core));
    let (addr, handle) = start_server(core_arc.clone(), None, None).await;
    let m = http_get(&addr, "/metrics").await;
    assert!(m.starts_with("HTTP/1.1 200"));
    assert!(m.contains("fusion_sv_up 1"));
    let h = http_get(&addr, "/health").await;
    assert!(h.starts_with("HTTP/1.1 200"));
    assert!(h.contains("{\"ok\":true}"));
    // 编排操作不在 GET 面, 未知路径 404
    let nf = http_get(&addr, "/api/v1/up").await;
    assert!(
        nf.starts_with("HTTP/1.1 404"),
        "GET up 应 404: {}",
        &nf[..30]
    );
    handle.abort();
}

#[tokio::test]
async fn test_history_and_events_read_store() {
    let core = SupervisorCore::new_with_fns(
        vec![def("fusion-mlx", 11434)],
        cfg(),
        |_| ProbeResult::Healthy { latency_ms: 3 },
        |_, _, _| Ok(()),
    );
    let core_arc = Arc::new(tokio::sync::Mutex::new(core));
    {
        let c = core_arc.lock().await;
        c.store
            .record_health("fusion-mlx", "Healthy", 12, 1000, "native")
            .unwrap();
        c.store
            .record_health("fusion-mlx", "Unhealthy", 0, 2000, "native")
            .unwrap();
        c.store
            .record_event("fusion-mlx", "Crashed", "exit 1", 1500, "native")
            .unwrap();
    }
    let (addr, handle) = start_server(core_arc.clone(), None, None).await;
    let h = http_get(&addr, "/api/v1/history?service=fusion-mlx").await;
    assert!(h.starts_with("HTTP/1.1 200"));
    let body = h.split("\r\n\r\n").nth(1).unwrap_or("");
    let v: serde_json::Value = serde_json::from_str(body).unwrap();
    let hist = v["history"].as_array().unwrap();
    assert_eq!(hist.len(), 2);
    assert_eq!(hist[0]["status"], "Healthy");
    assert_eq!(hist[0]["latency_ms"], 12);
    assert_eq!(hist[1]["status"], "Unhealthy");
    // 缺 service → 400
    let bad = http_get(&addr, "/api/v1/history").await;
    assert!(bad.starts_with("HTTP/1.1 400"));
    let e = http_get(&addr, "/api/v1/events?limit=10").await;
    let body = e.split("\r\n\r\n").nth(1).unwrap_or("");
    let v: serde_json::Value = serde_json::from_str(body).unwrap();
    let evs = v["events"].as_array().unwrap();
    assert_eq!(evs.len(), 1);
    assert_eq!(evs[0]["kind"], "Crashed");
    assert_eq!(evs[0]["detail"], "exit 1");
    handle.abort();
}

#[tokio::test]
async fn test_mutation_requires_token_and_reuses_dispatch() {
    // invoke_start 恒 Ok — up_all/restart 可执行; token 校验在 dispatch 层。
    let core = SupervisorCore::new_with_fns(
        vec![def("fusion-mlx", 11434)],
        cfg(),
        |_| ProbeResult::Healthy { latency_ms: 1 },
        |_, _, _| Ok(()),
    );
    let core_arc = Arc::new(tokio::sync::Mutex::new(core));
    let (addr, handle) = start_server(core_arc.clone(), Some("sekret".into()), None).await;

    // 无 token → dispatch 返回 unauthorized (HTTP 面透传错误)
    let no_auth = http_req(
        &addr,
        "POST /api/v1/up HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
    )
    .await;
    let body = no_auth.split("\r\n\r\n").nth(1).unwrap_or("");
    let v: serde_json::Value = serde_json::from_str(body).unwrap();
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unauthorized")
    );

    // Bearer token → 通过 dispatch 执行 up
    let ok = http_req(
        &addr,
        "POST /api/v1/up HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer sekret\r\nConnection: close\r\n\r\n",
    )
    .await;
    let body = ok.split("\r\n\r\n").nth(1).unwrap_or("");
    let v: serde_json::Value = serde_json::from_str(body).unwrap();
    assert!(!v["result"].is_null(), "up 应成功: {body}");
    assert!(v["error"].is_null());

    // 未知方法 → method not found
    let nf = http_req(
        &addr,
        "POST /api/v1/bogus HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer sekret\r\nConnection: close\r\n\r\n",
    )
    .await;
    let body = nf.split("\r\n\r\n").nth(1).unwrap_or("");
    assert!(body.contains("method not found"));
    handle.abort();
}

#[tokio::test]
async fn test_static_serving_and_traversal_rejected() {
    let core = SupervisorCore::new_with_fns(
        vec![def("fusion-mlx", 11434)],
        cfg(),
        |_| ProbeResult::Healthy { latency_ms: 1 },
        |_, _, _| Ok(()),
    );
    let core_arc = Arc::new(tokio::sync::Mutex::new(core));
    // 临时 dist: index.html + assets/app.js
    let dist = std::env::temp_dir().join(format!("fsv-web-test-{}", std::process::id()));
    std::fs::create_dir_all(dist.join("assets")).unwrap();
    std::fs::write(dist.join("index.html"), "<html>supervisor console</html>").unwrap();
    std::fs::write(dist.join("assets/app.js"), "console.log('ok')").unwrap();
    let (addr, handle) = start_server(
        core_arc.clone(),
        None,
        Some(dist.to_string_lossy().into_owned()),
    )
    .await;

    let idx = http_get(&addr, "/").await;
    assert!(idx.starts_with("HTTP/1.1 200"));
    assert!(idx.contains("text/html"));
    assert!(idx.contains("supervisor console"));
    let js = http_get(&addr, "/assets/app.js").await;
    assert!(js.starts_with("HTTP/1.1 200"));
    assert!(js.contains("text/javascript"));
    // 路径穿越 → 404
    let trav = http_get(&addr, "/../Cargo.toml").await;
    assert!(
        trav.starts_with("HTTP/1.1 404"),
        "穿越应 404: {}",
        &trav[..30]
    );
    // 存在性清理
    handle.abort();
    std::fs::remove_dir_all(&dist).unwrap();
}
