use crate::probe::ProbeSpec;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

// Service name → ProbeSpec. Paths confirmed against each service's live
// health endpoint by inspection (spec §4.3).
// Default is HTTP GET /health; exceptions override. UDS remains only for
// fusion-projects (speech/memory already moved to HTTP); services with an
// unconfirmed path degrade to TCP (one info log per svc, issue #104).
fn build_table() -> HashMap<&'static str, ProbeSpec> {
    let mut m = HashMap::new();
    // core
    // issue #79: under high load mlx holds the GIL so /healthz answers can exceed 2s —
    // the global 2s timeout would classify a healthy-but-busy service as Timeout →
    // repeated Unhealthy, escalating to a restart that kills in-flight inference.
    // Relaxed to 10s for mlx only.
    m.insert(
        "fusion-mlx",
        ProbeSpec::http_with_timeout(11434, "/healthz", 10),
    );
    m.insert("fusion-gateway", ProbeSpec::http(11432, "/healthz"));
    // platform
    m.insert("fusion-rag", ProbeSpec::http(11436, "/health"));
    // issue #34: the actual public health endpoint is /api/v1/system/health (auth.py PUBLIC_PATHS), root /health returns 401
    m.insert(
        "fusion-model-hub",
        ProbeSpec::http(11444, "/api/v1/system/health"),
    );
    // issue #34: fm-server HTTP face /healthz (socket already moved to ~/.fusion-memory/, HTTP probe is more robust)
    m.insert("fusion-memory", ProbeSpec::http(11435, "/healthz"));
    // app
    // issue #34: projectApiServer authenticates all endpoints (per-instance token), HTTP probe would be 401 forever → TCP
    m.insert("fusion-code", ProbeSpec::tcp(11441));
    m.insert("fusion-doc", ProbeSpec::http(11449, "/health"));
    m.insert("fusion-cowork", ProbeSpec::http(11438, "/health"));
    // issue #34: cowork-ws = `fusion-cowork remote serve` WebSocket, no /health → TCP
    m.insert("fusion-cowork-ws", ProbeSpec::tcp(11439));
    // issue #34: fusion-projects is a UDS service (sock /tmp/fusion-project-svc.sock),
    // registry 11440 is a dead value and TCP probe is unreachable forever → UDS probe
    m.insert(
        "fusion-projects",
        ProbeSpec::uds("/tmp/fusion-project-svc.sock"),
    );
    // domain
    // issue #34: routes are mounted under the /api/v1 prefix (api/app.py include_router), root /health 404
    m.insert("fusion-health", ProbeSpec::http(11469, "/api/v1/health"));
    // issue #34: APIKeyMiddleware exempt path is /api/v1/ready (503 = dependency not ready, real status)
    m.insert("fusion-finance", ProbeSpec::http(11466, "/api/v1/ready"));
    // issue #34: k12 health endpoint /api/health (all-pass only when backend=ok)
    m.insert("fusion-k12-teacher", ProbeSpec::http(11448, "/api/health"));
    // issue #34: bench public health endpoint /api/v1/system/health (auth exempt list)
    m.insert(
        "fusion-bench",
        ProbeSpec::http(11467, "/api/v1/system/health"),
    );
    // issue #34: modelization REST /health (same path as start.sh)
    m.insert(
        "fusion-code-modenization",
        ProbeSpec::http(11459, "/health"),
    );
    // issue #34: speech daemon socket moved to ~/.fusion-speech/run/ (upstream config.py),
    // HTTP /health face has always existed; switched to HTTP probe
    m.insert("fusion-speech", ProbeSpec::http(11465, "/health"));
    // issue #29: shell-ecosystem module services (/health verified live for each)
    m.insert("fusion-cowork-web", ProbeSpec::http(11461, "/health"));
    m.insert("fusion-trainer-bridge", ProbeSpec::http(8790, "/health"));
    m.insert(
        "fusion-plugins-ecosystem-console",
        ProbeSpec::http(8795, "/health"),
    );
    m.insert("fusion-agent-studio", ProbeSpec::http(11442, "/health"));
    // issue #88: fusion-security public health endpoint (start.sh header comment: auth exempt), better than the TCP fallback
    // (both /health and /api/v1/health sit behind auth and return 401 forever, unusable as probe paths)
    m.insert(
        "fusion-security",
        ProbeSpec::http(11454, "/api/v1/system/health"),
    );
    // issue #31: MCP server (sse) has no /health endpoint, TCP port probe
    m.insert("fusion-plugins-ecosystem", ProbeSpec::tcp(8765));
    // issue #33: fs-serve /health (deploy/start.sh, :11463)
    m.insert("fusion-store", ProbeSpec::http(11463, "/health"));
    // osagent issue#6: Prometheus scrape endpoint, /metrics unauthenticated, always 200 (:11571)
    m.insert("fusion-osagent-metrics", ProbeSpec::http(11571, "/metrics"));
    // issue #33: fsb uvicorn /health (:11490)
    m.insert("fusion-smallbusiness", ProbeSpec::http(11490, "/health"));
    // issue #33: simulation gRPC service has no HTTP face, TCP port probe (:11447)
    m.insert("fusion-simulation", ProbeSpec::tcp(11447));
    // issue #89: fusion-design (fd-server) health endpoint /api/health (router.rs, 200 = alive;
    // deep=1 is the deep probe, not used for health checks)
    m.insert("fusion-design", ProbeSpec::http(11493, "/api/health"));
    // issue #45: business platform probes — ai-fit public health endpoint (same auth exempt as server/main.py),
    // speech-exam /health unauthenticated; ai-family mounts verify_service_key on all routers (401 forever)
    // and apparelmockup (static http.server, no /health, 404 forever = BadPath, no restart) fall back to TCP
    m.insert("ai-fit", ProbeSpec::http(11510, "/api/v1/system/health"));
    m.insert("ai-speech-exam", ProbeSpec::http(11560, "/health"));
    // 2026-11-11 ai-family moved from 11483 to 11583 (registry synced, dahai80-business/
    // ai-family#1) — pinning the old port means the probe is ConnectionRefused forever
    // (restart-eligible), a healthy service would be killed by a restart storm
    m.insert("ai-family", ProbeSpec::tcp(11583));
    // issue #55: apparelmockup upstream switched to FastAPI but no public health endpoint seen → conservative TCP
    m.insert("ai-apparelmockup", ProbeSpec::tcp(11520));
    // issue #84: ai-trysuite public health endpoint /healthz (unauthenticated), better than the TCP fallback
    m.insert("ai-trysuite", ProbeSpec::http(11515, "/healthz"));
    // issue #55: batch-registered entries — ones with an auth-exempt /health use HTTP,
    // the rest (ads_vb / operabase have no public health endpoint, static http.server 404 forever) fall back to TCP
    m.insert("ai-doc-automation", ProbeSpec::http(11550, "/health"));
    // issue #88: two more business entries adopted in the fleet audit — lead-qualifier
    // /api/v1/system/health (app.py)
    // issue #143: ai-assistant-agent entry removed (self-healing service, no longer supervised)
    // issue #94: booking_agent/main.py:233 GET /health
    m.insert("ai-booking-agent", ProbeSpec::http(11516, "/health"));
    m.insert(
        "ai-lead-qualifier",
        ProbeSpec::http(11552, "/api/v1/system/health"),
    );
    m.insert("ai-information-search", ProbeSpec::http(11595, "/health"));
    m.insert("ai-charactervoice", ProbeSpec::http(11505, "/health"));
    m.insert(
        "ai-charactervoice-session",
        ProbeSpec::http(11506, "/health"),
    );
    m.insert("ai-ads-videobuilder", ProbeSpec::tcp(11531));
    m.insert("ai-OperaBase", ProbeSpec::tcp(11504));
    m.insert("customer-insight", ProbeSpec::tcp(11594));
    m.insert("customer-insight-api", ProbeSpec::tcp(11501));
    m.insert("ai-apparelmockup-web", ProbeSpec::tcp(11593));
    // issue #155/#163: fusion-site probe pinned to the site's native health endpoint. #155 had pinned the module face
    // /shell/fusion-site/api/experience/health, but since 2026-11-25 that path is taken over by the upstream
    // shell session middleware (401 session_required, a healthy site gets classified Unhealthy).
    // On the site origin (node :11481 direct), the non-prefixed /api/experience/health is unauthenticated and
    // returns real health JSON — the probe pins here. A dedicated unauthenticated upstream /healthz has an issue filed;
    // migrate once it lands.
    m.insert(
        "fusion-site",
        ProbeSpec::http(11481, "/api/experience/health"),
    );
    // issue #81: office-api has its own health endpoint (upstream start.sh curls it after startup,
    // services/office-api, :11492)
    m.insert("fusion-office-api", ProbeSpec::http(11492, "/healthz"));
    // issue #120: fusion-desigma GUI desktop app — no HTTP/TCP face,
    // alive = the process in the start.sh-managed pidfile (.fusion-desigma.pid) is still running
    m.insert(
        "fusion-desigma",
        ProbeSpec::pidfile("fusion-desigma/.fusion-desigma.pid"),
    );
    // issue #160: fusion-link connection daemon — no HTTP/TCP face (pure outbound WSS
    // client), alive = the process in the start.sh-managed pidfile is still running. The pidfile
    // lives under HOME (~/..); probe_pidfile supports ~ prefix expansion.
    m.insert(
        "fusion-link",
        ProbeSpec::pidfile("~/.fusion-link/fusion-link.pid"),
    );
    // Remaining services fall back: ProbeSpec::tcp(port) is filled in inside probe_spec_for
    m
}

static TABLE: OnceLock<HashMap<&'static str, ProbeSpec>> = OnceLock::new();

// issue #104: degradation is static configuration info — log once per service,
// never per poll cycle (a minimal machine has ~90 unconfirmed services; the
// old per-call WARN flooded the boot log every tick).
static DEGRADED_SEEN: OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    OnceLock::new();

pub fn probe_spec_for(name: &str, port: u16) -> ProbeSpec {
    let table = TABLE.get_or_init(build_table);
    if let Some(spec) = table.get(name) {
        return spec.clone();
    }
    // Fallback: TCP port reachable (weakest probe, path unconfirmed) — log one
    // info on first occurrence, silent afterwards
    let seen =
        DEGRADED_SEEN.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()));
    let mut seen = seen.lock().unwrap_or_else(|p| p.into_inner());
    if seen.insert(name.to_string()) {
        tracing::info!(
            svc = name,
            port,
            "probe_map path unconfirmed, degraded to TCP probe (add an entry in probe_map.rs to confirm)"
        );
    }
    ProbeSpec::tcp(port)
}

// fusion-store (daemonized direct-run since issue #33) has no restart semantics → degrade to stop+start.
// issue #29: start_map override services (trainer-bridge script has no restart subcommand;
// daemonized processes have no restart semantics) all stop+start.
pub fn restart_cmd_for(name: &str) -> &'static str {
    // issue #45: ai-family start.sh only has start/stop/status/log/doctor,
    // no restart subcommand (bash start.sh restart → usage exit 1) → stop+start
    // issue #83: ai-trysuite server/start.sh has the same usage (no restart)
    if name == "fusion-store"
        || name == "ai-family"
        || name == "ai-trysuite"
        // issue #89: fusion-design start.sh only has start|stop|status|log, no restart
        || name == "fusion-design"
        || crate::start_map::start_spec_for(name).is_some()
    {
        "stop+start"
    } else {
        "restart"
    }
}

pub fn start_sh_path(repo_dir: &str) -> PathBuf {
    let root = std::env::var("FUSION_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    root.join(repo_dir).join("start.sh")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::ProbeKind;

    #[test]
    fn test_probe_spec_known_http() {
        let s = probe_spec_for("fusion-mlx", 11434);
        assert!(matches!(s.kind, ProbeKind::Http));
        assert_eq!(s.path.as_deref(), Some("/healthz"));
    }

    #[test]
    fn test_probe_spec_known_uds() {
        // issue #34: after memory switched to HTTP, fusion-projects is the representative UDS probe
        let s = probe_spec_for("fusion-projects", 11440);
        assert!(matches!(s.kind, ProbeKind::Uds));
    }

    #[test]
    fn test_issue88_security_probe() {
        // issue #88: fusion-security auth-exempt health endpoint, no longer degraded to the TCP fallback
        let s = probe_spec_for("fusion-security", 11454);
        assert!(matches!(s.kind, ProbeKind::Http));
        assert_eq!(s.path.as_deref(), Some("/api/v1/system/health"));
        assert_eq!(s.port, 11454);
    }

    #[test]
    fn test_issue88_business_audit_probes() {
        // issue #88: health endpoints of the two business entries adopted in the fleet audit
        // issue #143: ai-assistant-agent removed (self-healing service, no longer supervised) — falls to the TCP fallback
        assert!(matches!(
            probe_spec_for("ai-assistant-agent", 11511).kind,
            ProbeKind::Tcp
        ));
        let l = probe_spec_for("ai-lead-qualifier", 11552);
        assert!(matches!(l.kind, ProbeKind::Http));
        assert_eq!(l.path.as_deref(), Some("/api/v1/system/health"));
    }

    // issue #89: fusion-design (fd-server) HTTP /api/health probe + start.sh has no
    // restart subcommand → stop+start
    #[test]
    fn test_issue89_design_probe() {
        let s = probe_spec_for("fusion-design", 11493);
        assert!(matches!(s.kind, ProbeKind::Http));
        assert_eq!(s.path.as_deref(), Some("/api/health"));
        assert_eq!(s.port, 11493);
        assert_eq!(restart_cmd_for("fusion-design"), "stop+start");
    }

    // issue #155: fusion-site module face basePath build intentionally 404s `/` —
    // the probe must pin /shell/fusion-site/api/experience/health
    #[test]
    fn test_issue163_fusion_site_unauth_probe() {
        let s = probe_spec_for("fusion-site", 11481);
        assert!(matches!(s.kind, ProbeKind::Http));
        // the module-face path pinned by #155 has been taken over by the upstream session middleware (401),
        // the probe must land on the site origin's unauthenticated health endpoint
        assert_eq!(s.path.as_deref(), Some("/api/experience/health"));
        assert_eq!(s.port, 11481);
        // start_map override services have no restart semantics
        assert_eq!(restart_cmd_for("fusion-site"), "stop+start");
    }

    #[test]
    fn test_probe_spec_fallback_tcp() {
        let s = probe_spec_for("fusion-fake-unknown", 11499);
        assert!(matches!(s.kind, ProbeKind::Tcp));
    }

    #[test]
    fn test_restart_cmd_store_special() {
        assert_eq!(restart_cmd_for("fusion-store"), "stop+start");
        assert_eq!(restart_cmd_for("fusion-mlx"), "restart");
    }

    // issue #29: start_map override services → stop+start (no restart semantics)
    #[test]
    fn test_restart_cmd_start_map_overrides() {
        assert_eq!(restart_cmd_for("fusion-trainer-bridge"), "stop+start");
        assert_eq!(restart_cmd_for("fusion-cowork-web"), "stop+start");
        assert_eq!(
            restart_cmd_for("fusion-plugins-ecosystem-console"),
            "stop+start"
        );
        // issue #83: ai-trysuite server/start.sh only has start|stop|status|log|doctor
        assert_eq!(restart_cmd_for("ai-trysuite"), "stop+start");
        // issue #84: ai-trysuite public health endpoint /healthz
        let s = probe_spec_for("ai-trysuite", 11515);
        assert!(matches!(s.kind, ProbeKind::Http));
        assert_eq!(s.path.as_deref(), Some("/healthz"));
    }

    // issue #31: MCP server has no /health, TCP probe
    #[test]
    fn test_plugins_ecosystem_tcp_probe() {
        let s = probe_spec_for("fusion-plugins-ecosystem", 8765);
        assert!(matches!(s.kind, ProbeKind::Tcp));
    }

    // issue #33: probes for newly supervised services — store/fsb HTTP /health, simulation gRPC TCP
    #[test]
    fn test_issue33_probe_specs() {
        for (name, port) in [("fusion-store", 11463), ("fusion-smallbusiness", 11490)] {
            let s = probe_spec_for(name, port);
            assert!(
                matches!(s.kind, ProbeKind::Http),
                "{name} should probe via HTTP"
            );
            assert_eq!(
                s.path.as_deref(),
                Some("/health"),
                "{name} path should be /health"
            );
        }
        let s = probe_spec_for("fusion-simulation", 11447);
        assert!(
            matches!(s.kind, ProbeKind::Tcp),
            "simulation should probe via TCP"
        );
    }

    // issue #33: all three newly supervised services are start_map overrides → restart degrades to stop+start
    #[test]
    fn test_issue33_restart_cmd_stop_start() {
        for name in ["fusion-store", "fusion-smallbusiness", "fusion-simulation"] {
            assert_eq!(restart_cmd_for(name), "stop+start", "{name}");
        }
    }

    // issue #81: office-api HTTP /healthz probe + start_map override → stop+start
    #[test]
    fn test_issue81_office_api_probe() {
        let s = probe_spec_for("fusion-office-api", 11492);
        assert!(
            matches!(s.kind, ProbeKind::Http),
            "office-api should probe via HTTP"
        );
        assert_eq!(s.path.as_deref(), Some("/healthz"));
        assert_eq!(restart_cmd_for("fusion-office-api"), "stop+start");
    }

    // issue #120: desigma GUI app PidFile probe (no HTTP/TCP face)
    #[test]
    fn test_issue120_desigma_pidfile_probe() {
        let s = probe_spec_for("fusion-desigma", 0);
        assert!(
            matches!(s.kind, ProbeKind::PidFile),
            "desigma should probe via PidFile"
        );
        assert_eq!(
            s.pidfile.as_deref(),
            Some("fusion-desigma/.fusion-desigma.pid")
        );
    }

    // issue #160: fusion-link connection daemon PidFile probe (pure outbound WSS, no HTTP/TCP face)
    #[test]
    fn test_issue160_fusion_link_pidfile_probe() {
        let s = probe_spec_for("fusion-link", 0);
        assert!(
            matches!(s.kind, ProbeKind::PidFile),
            "fusion-link should probe via PidFile"
        );
        assert_eq!(s.pidfile.as_deref(), Some("~/.fusion-link/fusion-link.pid"));
    }

    // issue #34: probe paths aligned with upstream reality — model-hub/health/finance switched to real paths,
    // speech/memory switched to HTTP, projects to UDS, code is 401 forever so falls back to TCP
    #[test]
    fn test_issue34_probe_path_fixes() {
        let s = probe_spec_for("fusion-model-hub", 11444);
        assert_eq!(s.path.as_deref(), Some("/api/v1/system/health"));

        let s = probe_spec_for("fusion-health", 11469);
        assert_eq!(s.path.as_deref(), Some("/api/v1/health"));

        let s = probe_spec_for("fusion-finance", 11466);
        assert_eq!(s.path.as_deref(), Some("/api/v1/ready"));

        for name in ["fusion-speech", "fusion-memory"] {
            let s = probe_spec_for(name, 1);
            assert!(
                matches!(s.kind, ProbeKind::Http),
                "{name} should probe via HTTP"
            );
        }
        let s = probe_spec_for("fusion-speech", 11465);
        assert_eq!(s.path.as_deref(), Some("/health"));
        let s = probe_spec_for("fusion-memory", 11435);
        assert_eq!(s.path.as_deref(), Some("/healthz"));

        let s = probe_spec_for("fusion-projects", 11440);
        assert!(
            matches!(s.kind, ProbeKind::Uds),
            "projects should probe via UDS"
        );

        let s = probe_spec_for("fusion-code", 11441);
        assert!(
            matches!(s.kind, ProbeKind::Tcp),
            "code is 401 forever, should probe via TCP"
        );

        let s = probe_spec_for("fusion-cowork-ws", 11439);
        assert!(matches!(s.kind, ProbeKind::Tcp));

        // issue #34 supplement: bench/k12/modenization switched to real paths
        let s = probe_spec_for("fusion-bench", 11467);
        assert_eq!(s.path.as_deref(), Some("/api/v1/system/health"));
        let s = probe_spec_for("fusion-k12-teacher", 11448);
        assert_eq!(s.path.as_deref(), Some("/api/health"));
        let s = probe_spec_for("fusion-code-modenization", 11459);
        assert_eq!(s.path.as_deref(), Some("/health"));
    }

    // issue #45: business platform probes — fit-ai/speech-exam HTTP, family/apparelmockup TCP
    #[test]
    fn test_issue42_business_probe_specs() {
        let s = probe_spec_for("ai-fit", 11510);
        assert!(matches!(s.kind, ProbeKind::Http));
        assert_eq!(s.path.as_deref(), Some("/api/v1/system/health"));

        let s = probe_spec_for("ai-speech-exam", 11560);
        assert!(matches!(s.kind, ProbeKind::Http));
        assert_eq!(s.path.as_deref(), Some("/health"));

        for (name, port) in [("ai-family", 11583), ("ai-apparelmockup", 11520)] {
            let s = probe_spec_for(name, port);
            assert!(
                matches!(s.kind, ProbeKind::Tcp),
                "{name} should probe via TCP"
            );
        }
    }

    // issue #45: ai-family has no restart subcommand → stop+start; start_map-overridden
    // business services (fit-ai etc.) also stop+start
    #[test]
    fn test_issue42_restart_cmd_business() {
        assert_eq!(restart_cmd_for("ai-family"), "stop+start");
        assert_eq!(restart_cmd_for("ai-fit"), "stop+start");
        assert_eq!(restart_cmd_for("ai-speech-exam"), "stop+start");
        assert_eq!(restart_cmd_for("ai-apparelmockup"), "stop+start");
        // issue #55: new batch entries are also start_map overrides → stop+start
        assert_eq!(restart_cmd_for("ai-charactervoice"), "stop+start");
        assert_eq!(restart_cmd_for("customer-insight"), "stop+start");
    }

    #[test]
    fn test_shell_ecosystem_probe_specs() {
        for (name, port) in [
            ("fusion-cowork-web", 11461),
            ("fusion-trainer-bridge", 8790),
            ("fusion-plugins-ecosystem-console", 8795),
            ("fusion-agent-studio", 11442),
        ] {
            let s = probe_spec_for(name, port);
            assert!(
                matches!(s.kind, ProbeKind::Http),
                "{name} should probe via HTTP"
            );
            assert_eq!(
                s.path.as_deref(),
                Some("/health"),
                "{name} path should be /health"
            );
        }
    }
}
