use std::time::Duration;

// Four probe kinds: HTTP GET / UDS ping / TCP port reachable / PidFile process alive. 2s timeout (aligned with fusion-cli).
// Anti-storm: BadPath(404/405) = wrong path, no restart; only ConnectionRefused/Timeout enter the restart flow.
#[derive(Debug, Clone, Copy)]
pub enum ProbeKind {
    Http,
    Uds,
    Tcp,
    PidFile,
}

#[derive(Debug, Clone)]
pub struct ProbeSpec {
    pub kind: ProbeKind,
    pub port: u16,
    pub path: Option<String>,
    pub socket: Option<String>,
    // issue #120: PidFile probe — GUI desktop apps have no HTTP/TCP face, alive = process exists.
    // Relative paths resolve against FUSION_ROOT (start.sh owns the pidfile, not the supervisor).
    pub pidfile: Option<String>,
    // issue #79: per-service probe timeout override. None = global default 2s.
    // GIL-bound inference services (fusion-mlx) under load may answer /healthz in >2s;
    // a 2s timeout misjudges a healthy-but-busy service as down and triggers a restart
    // (killing in-flight traffic).
    pub timeout: Option<Duration>,
}

impl ProbeSpec {
    pub fn http(port: u16, path: &str) -> Self {
        Self {
            kind: ProbeKind::Http,
            port,
            path: Some(path.to_string()),
            socket: None,
            pidfile: None,
            timeout: None,
        }
    }
    // issue #79: HTTP probe spec with a timeout override
    pub fn http_with_timeout(port: u16, path: &str, secs: u64) -> Self {
        Self {
            timeout: Some(Duration::from_secs(secs)),
            ..Self::http(port, path)
        }
    }
    pub fn uds(socket: &str) -> Self {
        Self {
            kind: ProbeKind::Uds,
            port: 0,
            path: None,
            socket: Some(socket.to_string()),
            pidfile: None,
            timeout: None,
        }
    }
    pub fn tcp(port: u16) -> Self {
        Self {
            kind: ProbeKind::Tcp,
            port,
            path: None,
            socket: None,
            pidfile: None,
            timeout: None,
        }
    }

    // issue #120: GUI desktop app (fusion-desigma) — no HTTP/TCP face,
    // alive = the process in the start.sh-owned pidfile still exists.
    pub fn pidfile(path: &str) -> Self {
        Self {
            kind: ProbeKind::PidFile,
            port: 0,
            path: None,
            socket: None,
            pidfile: Some(path.to_string()),
            timeout: None,
        }
    }

    // Effective timeout: spec override > global default
    fn effective_timeout(&self) -> Duration {
        self.timeout.unwrap_or(PROBE_TIMEOUT)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnhealthyReason {
    ConnectionRefused,
    Timeout,
    BadPath(u16),
    BadStatus(u16),
    UdsConnectFailed,
    UdsBadReply,
    // issue #111: the pidfile process is dead but the probe still passes — the port is held by a
    // stale listener (orphaned parent / another process colliding), probe-pass ≠ process-alive.
    // Treated as a real crash, entering auto-restart (#96 port fallback + #110 ownership check clear the stale listener).
    ProcessExit,
}

impl UnhealthyReason {
    // Only these = real crash signals, trigger restart. The rest never restart (anti-storm).
    pub fn restart_eligible(&self) -> bool {
        matches!(
            self,
            UnhealthyReason::ConnectionRefused
                | UnhealthyReason::Timeout
                | UnhealthyReason::ProcessExit
        )
    }
}

#[derive(Debug, Clone)]
pub enum ProbeResult {
    Healthy { latency_ms: u64 },
    Unhealthy { reason: UnhealthyReason },
}

const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

// Pure function: classify an HTTP status. 2xx=Healthy, 404/405=BadPath, else=BadStatus.
pub fn classify_status(status: u16) -> ProbeResult {
    if (200..300).contains(&status) {
        ProbeResult::Healthy { latency_ms: 0 }
    } else if status == 404 || status == 405 {
        ProbeResult::Unhealthy {
            reason: UnhealthyReason::BadPath(status),
        }
    } else {
        ProbeResult::Unhealthy {
            reason: UnhealthyReason::BadStatus(status),
        }
    }
}

// issue #120: PidFile probe — read the pidfile, kill -0 for process liveness. Relative paths
// resolve against FUSION_ROOT (fallback CWD); `~/` prefixes expand against HOME (issue #160: the
// fusion-link pidfile lives under $HOME/.fusion-link/). Missing/unreadable pidfile/dead process →
// ProcessExit (restart-eligible: GUI crashes get auto-restarted, the circuit-breaker window caps storms).
async fn probe_pidfile(spec: &ProbeSpec) -> ProbeResult {
    let raw = spec.pidfile.as_deref().unwrap_or_default();
    let path = if let Some(rest) = raw.strip_prefix("~/") {
        match std::env::var("HOME") {
            Ok(home) if !home.is_empty() => std::path::PathBuf::from(home).join(rest),
            _ => std::path::PathBuf::from(raw),
        }
    } else if raw.starts_with('/') {
        std::path::PathBuf::from(raw)
    } else {
        match std::env::var("FUSION_ROOT") {
            Ok(root) if !root.is_empty() => std::path::PathBuf::from(root).join(raw),
            _ => std::path::PathBuf::from(raw),
        }
    };
    let pid = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok());
    let pid = match pid {
        Some(p) => p,
        None => {
            tracing::debug!(pidfile = %path.display(), "pidfile probe: missing or unparseable");
            return ProbeResult::Unhealthy {
                reason: UnhealthyReason::ProcessExit,
            };
        }
    };
    let start = std::time::Instant::now();
    let alive = tokio::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|st| st.success())
        .unwrap_or(false);
    if alive {
        ProbeResult::Healthy {
            latency_ms: start.elapsed().as_millis() as u64,
        }
    } else {
        ProbeResult::Unhealthy {
            reason: UnhealthyReason::ProcessExit,
        }
    }
}

pub async fn probe(spec: &ProbeSpec) -> ProbeResult {
    match spec.kind {
        ProbeKind::Http => probe_http(spec).await,
        ProbeKind::Uds => probe_uds(spec).await,
        ProbeKind::Tcp => probe_tcp(spec).await,
        ProbeKind::PidFile => probe_pidfile(spec).await,
    }
}

async fn probe_http(spec: &ProbeSpec) -> ProbeResult {
    let path = spec.path.as_deref().unwrap_or("/health");
    let url = format!("http://127.0.0.1:{}{}", spec.port, path);
    // issue #150: loopback probes must never use the system proxy — when macOS has a system proxy
    // enabled (clash :7897), reqwest reads SystemConfiguration by default and 127.0.0.1 /healthz
    // gets intercepted by clash returning 502, misjudging healthy services as down and triggering
    // restart storms. Force no_proxy, aligning with curl's direct-connect behavior.
    let client = reqwest::Client::builder()
        .timeout(spec.effective_timeout())
        .no_proxy()
        .build()
        .unwrap_or_default();
    let start = std::time::Instant::now();
    match client.get(&url).send().await {
        Ok(resp) => {
            let latency = start.elapsed().as_millis() as u64;
            let status = resp.status().as_u16();
            match classify_status(status) {
                ProbeResult::Healthy { .. } => ProbeResult::Healthy {
                    latency_ms: latency,
                },
                other => other,
            }
        }
        Err(e) => {
            // issue #104: routine probe failures must not WARN per poll cycle —
            // an absent/never-started service fails here every tick and a
            // minimal machine (~90 absent services) flooded the boot log.
            // Failure signal is carried by the runner state machine
            // (transition-gated alerts, #49); keep this at debug.
            tracing::debug!(url = %url, err = %e, "http probe fail");
            if e.is_connect() {
                ProbeResult::Unhealthy {
                    reason: UnhealthyReason::ConnectionRefused,
                }
            } else if e.is_timeout() {
                ProbeResult::Unhealthy {
                    reason: UnhealthyReason::Timeout,
                }
            } else {
                ProbeResult::Unhealthy {
                    reason: UnhealthyReason::ConnectionRefused,
                }
            }
        }
    }
}

async fn probe_uds(spec: &ProbeSpec) -> ProbeResult {
    #[cfg(unix)]
    {
        probe_uds_unix(spec).await
    }
    // windows has no UnixStream — UDS probe permanently unreachable (non-restart kind)
    #[cfg(not(unix))]
    {
        let _ = spec;
        ProbeResult::Unhealthy {
            reason: UnhealthyReason::UdsConnectFailed,
        }
    }
}

#[cfg(unix)]
async fn probe_uds_unix(spec: &ProbeSpec) -> ProbeResult {
    let sock = spec.socket.as_deref().unwrap_or("/tmp/fusion-sv.sock");
    let conn = tokio::time::timeout(
        spec.effective_timeout(),
        tokio::net::UnixStream::connect(sock),
    )
    .await;
    let mut stream = match conn {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            // issue #104: same per-poll spam concern as the http arm — debug.
            tracing::debug!(sock = %sock, err = %e, "uds connect fail");
            return ProbeResult::Unhealthy {
                reason: UnhealthyReason::UdsConnectFailed,
            };
        }
        Err(_) => {
            return ProbeResult::Unhealthy {
                reason: UnhealthyReason::Timeout,
            };
        }
    };
    // Send the repo's existing envelope ping. The protocol is newline-delimited (the server reads
    // via lines()/readline()); without \n the peer cannot parse. And a reply must be read to count
    // as alive — write-only would let a dispatch-stuck daemon pass (fake Healthy).
    let req = "{\"jsonrpc\":\"2.0\",\"method\":\"ping\",\"id\":1}\n";
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    if stream.write_all(req.as_bytes()).await.is_err() {
        return ProbeResult::Unhealthy {
            reason: UnhealthyReason::UdsBadReply,
        };
    }
    let mut buf = [0u8; 512];
    match tokio::time::timeout(spec.effective_timeout(), stream.read(&mut buf)).await {
        // any non-empty reply (result/error both count) = daemon dispatch alive
        Ok(Ok(n)) if n > 0 => ProbeResult::Healthy { latency_ms: 0 },
        _ => ProbeResult::Unhealthy {
            reason: UnhealthyReason::UdsBadReply,
        },
    }
}

async fn probe_tcp(spec: &ProbeSpec) -> ProbeResult {
    let addr = format!("127.0.0.1:{}", spec.port);
    match tokio::time::timeout(
        spec.effective_timeout(),
        tokio::net::TcpStream::connect(&addr),
    )
    .await
    {
        Ok(Ok(_)) => ProbeResult::Healthy { latency_ms: 0 },
        Ok(Err(_)) => ProbeResult::Unhealthy {
            reason: UnhealthyReason::ConnectionRefused,
        },
        Err(_) => ProbeResult::Unhealthy {
            reason: UnhealthyReason::Timeout,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_probe_spec_http() {
        let s = ProbeSpec::http(11434, "/healthz");
        assert!(matches!(s.kind, ProbeKind::Http));
        assert_eq!(s.port, 11434);
        assert_eq!(s.path.as_deref(), Some("/healthz"));
    }

    #[test]
    fn test_probe_spec_uds() {
        let s = ProbeSpec::uds("/tmp/fusion-sv.sock");
        assert!(matches!(s.kind, ProbeKind::Uds));
        assert_eq!(s.socket.as_deref(), Some("/tmp/fusion-sv.sock"));
    }

    // issue #79: timeout override constructor + default None (global 2s)
    #[test]
    fn test_probe_spec_timeout_override() {
        let s = ProbeSpec::http(11434, "/healthz");
        assert_eq!(s.timeout, None);
        assert_eq!(s.effective_timeout(), PROBE_TIMEOUT);
        let s = ProbeSpec::http_with_timeout(11434, "/healthz", 10);
        assert_eq!(s.timeout, Some(Duration::from_secs(10)));
        assert_eq!(s.effective_timeout(), Duration::from_secs(10));
        // non-overriding constructors unaffected
        assert_eq!(ProbeSpec::tcp(1).timeout, None);
        assert_eq!(ProbeSpec::uds("/tmp/x.sock").timeout, None);
    }

    #[test]
    fn test_unhealthy_reason_restart_eligible() {
        // only these enter the restart flow
        assert!(UnhealthyReason::ConnectionRefused.restart_eligible());
        assert!(UnhealthyReason::Timeout.restart_eligible());
        // BadPath never restarts (anti-storm)
        assert!(!UnhealthyReason::BadPath(404).restart_eligible());
        assert!(!UnhealthyReason::BadStatus(500).restart_eligible());
        assert!(!UnhealthyReason::UdsConnectFailed.restart_eligible());
    }

    #[tokio::test]
    async fn test_probe_http_healthy() {
        // connect reqwest to a nonexistent port → ConnectionRefused (no external server needed)
        // here we verify the classify_status pure function
        assert!(matches!(classify_status(200), ProbeResult::Healthy { .. }));
        assert!(matches!(classify_status(204), ProbeResult::Healthy { .. }));
        assert!(matches!(
            classify_status(500),
            ProbeResult::Unhealthy {
                reason: UnhealthyReason::BadStatus(500)
            }
        ));
        assert!(matches!(
            classify_status(404),
            ProbeResult::Unhealthy {
                reason: UnhealthyReason::BadPath(404)
            }
        ));
        assert!(matches!(
            classify_status(405),
            ProbeResult::Unhealthy {
                reason: UnhealthyReason::BadPath(405)
            }
        ));
    }

    // issue #120: PidFile probe three states — live process Healthy / dead pid ProcessExit / missing file ProcessExit
    #[tokio::test]
    async fn test_pidfile_probe_alive_process() {
        let tmp = tempfile::TempDir::new().unwrap();
        let pf = tmp.path().join("x.pid");
        std::fs::write(&pf, format!("{}", std::process::id())).unwrap();
        let spec = ProbeSpec::pidfile(pf.to_str().unwrap());
        assert!(matches!(probe(&spec).await, ProbeResult::Healthy { .. }));
    }

    #[tokio::test]
    async fn test_pidfile_probe_dead_process() {
        let tmp = tempfile::TempDir::new().unwrap();
        let pf = tmp.path().join("x.pid");
        // spawn a child that exits immediately to get a definitely-dead pid
        let mut child = std::process::Command::new("true")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let _ = child.wait();
        std::fs::write(&pf, format!("{pid}")).unwrap();
        let spec = ProbeSpec::pidfile(pf.to_str().unwrap());
        match probe(&spec).await {
            ProbeResult::Unhealthy {
                reason: UnhealthyReason::ProcessExit,
            } => {}
            other => panic!("expect ProcessExit, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_pidfile_probe_missing_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let spec = ProbeSpec::pidfile(tmp.path().join("absent.pid").to_str().unwrap());
        match probe(&spec).await {
            ProbeResult::Unhealthy {
                reason: UnhealthyReason::ProcessExit,
            } => {}
            other => panic!("expect ProcessExit, got {other:?}"),
        }
    }

    // issue #160: `~/`-prefixed pidfile expands against HOME (the fusion-link pidfile lives
    // under $HOME/.fusion-link/; a literal ~ left unexpanded makes the probe always ProcessExit)
    #[tokio::test]
    async fn test_issue160_pidfile_tilde_expansion() {
        let tmp = tempfile::TempDir::new().unwrap();
        let pf = tmp.path().join("fusion-link.pid");
        std::fs::write(&pf, format!("{}", std::process::id())).unwrap();
        let _guard = crate::env_map::tests::EnvGuard::set("HOME", tmp.path().to_str().unwrap());
        let spec = ProbeSpec::pidfile("~/fusion-link.pid");
        assert!(matches!(probe(&spec).await, ProbeResult::Healthy { .. }));
    }
}
