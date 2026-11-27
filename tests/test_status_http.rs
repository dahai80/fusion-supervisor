// issue #26/#25: HTTP face integration tests.
// Spawns the lib's http_server::serve directly (real implementation, not a test copy) and
// verifies: status mirroring / metrics untouched / history+events / orchestration token
// validation / web static files + traversal rejection / unknown paths 404.
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
        managed: true,
        boot_tier: fusion_supervisor::manifest::BootTier::T2,
        layer: Layer::App,
    }
}

fn cfg() -> fusion_supervisor::config::Config {
    fusion_supervisor::config::Config::default()
}

// Real-implementation server: random port, optional token / web_dir.
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
    // listener may not be ready right after spawn — retry connect (up to ~1s).
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
    let mut stream = stream.expect("server not ready (connect failed within 1s)");
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
    assert!(
        resp.starts_with("HTTP/1.1 200"),
        "expected 200: {}",
        &resp[..30]
    );
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
    // orchestration ops are not on the GET face; unknown paths 404
    let nf = http_get(&addr, "/api/v1/up").await;
    assert!(
        nf.starts_with("HTTP/1.1 404"),
        "GET up should 404: {}",
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
            .record_event("fusion-mlx", "Crashed", "exit 1", 1500, "native", "warn")
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
    // missing service → 400
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
    // invoke_start always Ok — up_all/restart execute; token validation happens in dispatch.
    let core = SupervisorCore::new_with_fns(
        vec![def("fusion-mlx", 11434)],
        cfg(),
        |_| ProbeResult::Healthy { latency_ms: 1 },
        |_, _, _| Ok(()),
    );
    let core_arc = Arc::new(tokio::sync::Mutex::new(core));
    let (addr, handle) = start_server(core_arc.clone(), Some("sekret".into()), None).await;

    // no token → dispatch returns unauthorized (HTTP face passes the error through)
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

    // Bearer token → up executes via dispatch
    let ok = http_req(
        &addr,
        "POST /api/v1/up HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer sekret\r\nConnection: close\r\n\r\n",
    )
    .await;
    let body = ok.split("\r\n\r\n").nth(1).unwrap_or("");
    let v: serde_json::Value = serde_json::from_str(body).unwrap();
    assert!(!v["result"].is_null(), "up should succeed: {body}");
    assert!(v["error"].is_null());

    // unknown method → method not found
    let nf = http_req(
        &addr,
        "POST /api/v1/bogus HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer sekret\r\nConnection: close\r\n\r\n",
    )
    .await;
    let body = nf.split("\r\n\r\n").nth(1).unwrap_or("");
    assert!(body.contains("method not found"));
    handle.abort();
}

// 2026-10-31: fleet-wide up and single-service up both record origin events (symmetric
// with the down side, #82) — the 01:05 fleet-wide start wave previously left zero trace
// in logs/event table; the trigger could not be attributed.
#[tokio::test]
async fn test_up_records_startall_and_manualstart_events() {
    let core = SupervisorCore::new_with_fns(
        vec![def("fusion-mlx", 11434), def("fusion-rag", 11436)],
        cfg(),
        |_| ProbeResult::Healthy { latency_ms: 1 },
        |_, _, _| Ok(()),
    );
    let core_arc = Arc::new(tokio::sync::Mutex::new(core));
    let (addr, handle) = start_server(core_arc.clone(), Some("sekret".into()), None).await;

    // single-service up (JSON body) → ManualStart event
    let body = r#"{"service":"fusion-mlx"}"#;
    let req = format!(
        "POST /api/v1/up HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer sekret\r\nConnection: close\r\n\r\n{body}"
    );
    let resp = http_req(&addr, &req).await;
    assert!(resp.starts_with("HTTP/1.1 200"), "{}", &resp[..30]);

    // fleet-wide up (no body) → StartAll event (service="*")
    let all = http_req(
        &addr,
        "POST /api/v1/up HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer sekret\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(all.starts_with("HTTP/1.1 200"), "{}", &all[..30]);

    {
        let c = core_arc.lock().await;
        let evs = c.store.recent_events_all(10).unwrap();
        let manual = evs
            .iter()
            .find(|e| e.kind == "ManualStart")
            .expect("ManualStart event missing");
        assert_eq!(manual.service, "fusion-mlx");
        assert!(
            manual.detail.contains("http POST /api/v1/up from"),
            "origin should carry an http source: {}",
            manual.detail
        );
        let star = evs
            .iter()
            .find(|e| e.kind == "StartAll")
            .expect("StartAll event missing");
        assert_eq!(star.service, "*");
        assert!(star.detail.contains("http POST /api/v1/up from"));
    }
    handle.abort();
}

// issue #128: fleet-wide down (STOP-ALL) must record a ManualStop per service — previously
// only one StopAll("*") aggregate event was written, leaving stale notes on per-service
// last_restart (e.g. AutoRestart "restart ok"); fleet drift triage could not attribute
// who stopped a service.
#[tokio::test]
async fn test_stopall_records_per_service_manualstop_events() {
    let core = SupervisorCore::new_with_fns(
        vec![def("fusion-mlx", 11434), def("fusion-rag", 11436)],
        cfg(),
        |_| ProbeResult::Healthy { latency_ms: 1 },
        |_, _, _| Ok(()),
    );
    let core_arc = Arc::new(tokio::sync::Mutex::new(core));
    let (addr, handle) = start_server(core_arc.clone(), Some("sekret".into()), None).await;

    // fleet-wide up first so runners enter Healthy (was_running snapshot non-empty)
    let up = http_req(
        &addr,
        "POST /api/v1/up HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer sekret\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(up.starts_with("HTTP/1.1 200"), "{}", &up[..30]);

    // fleet-wide down (no body) → StopAll("*") + one ManualStop per previously running service
    let down = http_req(
        &addr,
        "POST /api/v1/down HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer sekret\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(down.starts_with("HTTP/1.1 200"), "{}", &down[..30]);

    {
        let c = core_arc.lock().await;
        let evs = c.store.recent_events_all(20).unwrap();
        for svc in ["fusion-mlx", "fusion-rag"] {
            let ev = evs
                .iter()
                .find(|e| e.kind == "ManualStop" && e.service == svc)
                .unwrap_or_else(|| panic!("{svc} missing ManualStop event"));
            assert!(
                ev.detail.contains("http POST /api/v1/down from"),
                "origin should carry an http source: {}",
                ev.detail
            );
        }
        assert!(
            evs.iter().any(|e| e.kind == "StopAll" && e.service == "*"),
            "StopAll aggregate event missing"
        );
    }
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
    // temp dist: index.html + assets/app.js
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
    // path traversal → 404
    let trav = http_get(&addr, "/../Cargo.toml").await;
    assert!(
        trav.starts_with("HTTP/1.1 404"),
        "traversal should 404: {}",
        &trav[..30]
    );
    // existence cleanup
    handle.abort();
    std::fs::remove_dir_all(&dist).unwrap();
}

// issue #139: external alert ingestion — token validation + event persistence +
// dedup_key window dedup; events readable via /api/v1/events (plane="external").
#[tokio::test]
async fn test_alert_ingest_token_record_and_dedup() {
    let core = SupervisorCore::new_with_fns(
        vec![def("fusion-mlx", 11434)],
        cfg(),
        |_| ProbeResult::Healthy { latency_ms: 1 },
        |_, _, _| Ok(()),
    );
    let core_arc = Arc::new(tokio::sync::Mutex::new(core));
    let (addr, handle) = start_server(core_arc.clone(), Some("sekret".into()), None).await;
    let body = r#"{"source":"fusion-ops","kind":"fail","severity":"error","detail":"probe down","dedup_key":"ops:11400"}"#;

    // no token → 401, no event recorded
    let no_auth = http_req(
        &addr,
        &format!(
            "POST /api/v1/alerts/ingest HTTP/1.1\r\nContent-Type: application/json\r\n\r\n{body}"
        ),
    )
    .await;
    assert!(no_auth.starts_with("HTTP/1.1 401"));
    // wrong token → 401
    let bad_auth = http_req(
        &addr,
        &format!("POST /api/v1/alerts/ingest HTTP/1.1\r\nx-token: wrong\r\n\r\n{body}"),
    )
    .await;
    assert!(bad_auth.starts_with("HTTP/1.1 401"));

    // valid token → 200 records the event; same dedup_key within the window → deduped:true, no duplicate
    let ok = http_req(
        &addr,
        &format!("POST /api/v1/alerts/ingest HTTP/1.1\r\nx-token: sekret\r\n\r\n{body}"),
    )
    .await;
    assert!(ok.starts_with("HTTP/1.1 200"));
    assert!(ok.contains("\"ok\":true"));
    let dup = http_req(
        &addr,
        &format!(
            "POST /api/v1/alerts/ingest HTTP/1.1\r\nAuthorization: Bearer sekret\r\n\r\n{body}"
        ),
    )
    .await;
    assert!(dup.starts_with("HTTP/1.1 200"));
    assert!(dup.contains("\"deduped\":true"));

    // bad body → 400
    let bad = http_req(
        &addr,
        "POST /api/v1/alerts/ingest HTTP/1.1\r\nx-token: sekret\r\n\r\n{\"kind\":\"k\"}",
    )
    .await;
    assert!(bad.starts_with("HTTP/1.1 400"));

    // exactly one event, plane=external, severity passed through
    let e = http_get(&addr, "/api/v1/events?limit=10").await;
    let v: serde_json::Value =
        serde_json::from_str(e.split("\r\n\r\n").nth(1).unwrap_or("")).unwrap();
    let evs = v["events"].as_array().unwrap();
    assert_eq!(evs.len(), 1);
    assert_eq!(evs[0]["service"], "fusion-ops");
    assert_eq!(evs[0]["kind"], "fail");
    assert_eq!(evs[0]["severity"], "error");
    assert_eq!(evs[0]["plane"], "external");
    assert_eq!(evs[0]["detail"], "probe down");
    handle.abort();
}
