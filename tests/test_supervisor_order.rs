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
        layer,
    }
}

#[tokio::test]
async fn test_up_order_core_before_app() {
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
}
