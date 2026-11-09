use fusion_supervisor::config::Config;
use fusion_supervisor::manifest::{Layer, ServiceDef};
use fusion_supervisor::probe::{ProbeResult, UnhealthyReason};
use fusion_supervisor::supervisor::SupervisorCore;
use std::sync::{Arc, Mutex};

fn def(name: &str, layer: Layer, port: u16) -> ServiceDef {
    ServiceDef {
        name: name.into(),
        port,
        repo_dir: name.into(),
        purpose: "x".into(),
        fixed: false,
        managed: true,
        boot_tier: fusion_supervisor::manifest::BootTier::T2,
        layer,
    }
}

#[tokio::test]
async fn test_up_order_core_before_app() {
    // Pin FUSION_CLIENT_KEY: the up path resolves it fail-closed for every
    // managed service (issue #85 inversion). CI runners have no gateway config
    // fallback, so fusion-rag's start would be refused before the start fn is
    // called. Same fix as the env_map / supervisor unit tests.
    let prev_key = std::env::var("FUSION_CLIENT_KEY").ok();
    // SAFETY: single-threaded current_thread tokio test; no concurrent env readers
    unsafe { std::env::set_var("FUSION_CLIENT_KEY", "ck-order-test") };
    let defs = vec![
        def("fusion-rag", Layer::App, 11436),
        def("fusion-mlx", Layer::Core, 11434),
    ];
    let cfg = Config {
        max_concurrent_start: 2,
        start_timeout_sec: 5,
        ..Config::default()
    };
    let order = Arc::new(Mutex::new(Vec::<String>::new()));
    let o = order.clone();
    let mut core = SupervisorCore::new_with_fns(
        defs,
        cfg,
        |_| ProbeResult::Unhealthy {
            reason: UnhealthyReason::ConnectionRefused,
        },
        move |svc, _c, _env| {
            o.lock().unwrap().push(svc.into());
            Ok(())
        },
    );
    core.up_all().await.unwrap();
    let seq = order.lock().unwrap();
    assert_eq!(seq[0], "fusion-mlx");
    assert_eq!(seq[1], "fusion-rag");
    drop(seq);
    match prev_key {
        Some(v) => unsafe { std::env::set_var("FUSION_CLIENT_KEY", v) },
        None => unsafe { std::env::remove_var("FUSION_CLIENT_KEY") },
    }
}
