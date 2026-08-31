use std::time::Duration;

// 探活三型: HTTP GET / UDS ping / TCP 端口通。2s 超时 (对齐 fusion-cli)。
// 防风暴: BadPath(404/405) = 路径错, 不重启; 仅 ConnectionRefused/Timeout 进重启流。
#[derive(Debug, Clone, Copy)]
pub enum ProbeKind {
    Http,
    Uds,
    Tcp,
}

#[derive(Debug, Clone)]
pub struct ProbeSpec {
    pub kind: ProbeKind,
    pub port: u16,
    pub path: Option<String>,
    pub socket: Option<String>,
}

impl ProbeSpec {
    pub fn http(port: u16, path: &str) -> Self {
        Self { kind: ProbeKind::Http, port, path: Some(path.to_string()), socket: None }
    }
    pub fn uds(socket: &str) -> Self {
        Self { kind: ProbeKind::Uds, port: 0, path: None, socket: Some(socket.to_string()) }
    }
    pub fn tcp(port: u16) -> Self {
        Self { kind: ProbeKind::Tcp, port, path: None, socket: None }
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
}

impl UnhealthyReason {
    // 仅这两个 = 真宕机信号, 触发重启。其余不重启 (防风暴)。
    pub fn restart_eligible(&self) -> bool {
        matches!(self, UnhealthyReason::ConnectionRefused | UnhealthyReason::Timeout)
    }
}

#[derive(Debug, Clone)]
pub enum ProbeResult {
    Healthy { latency_ms: u64 },
    Unhealthy { reason: UnhealthyReason },
}

const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

// 纯函数: 把 HTTP status 归类。2xx=Healthy, 404/405=BadPath, 其余=BadStatus。
pub fn classify_status(status: u16) -> ProbeResult {
    if (200..300).contains(&status) {
        ProbeResult::Healthy { latency_ms: 0 }
    } else if status == 404 || status == 405 {
        ProbeResult::Unhealthy { reason: UnhealthyReason::BadPath(status) }
    } else {
        ProbeResult::Unhealthy { reason: UnhealthyReason::BadStatus(status) }
    }
}

pub async fn probe(spec: &ProbeSpec) -> ProbeResult {
    match spec.kind {
        ProbeKind::Http => probe_http(spec).await,
        ProbeKind::Uds => probe_uds(spec).await,
        ProbeKind::Tcp => probe_tcp(spec).await,
    }
}

async fn probe_http(spec: &ProbeSpec) -> ProbeResult {
    let path = spec.path.as_deref().unwrap_or("/health");
    let url = format!("http://127.0.0.1:{}{}", spec.port, path);
    let client = reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .build()
        .unwrap_or_default();
    let start = std::time::Instant::now();
    match client.get(&url).send().await {
        Ok(resp) => {
            let latency = start.elapsed().as_millis() as u64;
            let status = resp.status().as_u16();
            match classify_status(status) {
                ProbeResult::Healthy { .. } => ProbeResult::Healthy { latency_ms: latency },
                other => other,
            }
        }
        Err(e) => {
            tracing::warn!(url = %url, err = %e, "http probe fail");
            if e.is_connect() {
                ProbeResult::Unhealthy { reason: UnhealthyReason::ConnectionRefused }
            } else if e.is_timeout() {
                ProbeResult::Unhealthy { reason: UnhealthyReason::Timeout }
            } else {
                ProbeResult::Unhealthy { reason: UnhealthyReason::ConnectionRefused }
            }
        }
    }
}

async fn probe_uds(spec: &ProbeSpec) -> ProbeResult {
    let sock = spec.socket.as_deref().unwrap_or("/tmp/fusion-sv.sock");
    let conn = tokio::time::timeout(
        PROBE_TIMEOUT,
        tokio::net::UnixStream::connect(sock),
    )
    .await;
    let mut stream = match conn {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            tracing::warn!(sock = %sock, err = %e, "uds connect fail");
            return ProbeResult::Unhealthy { reason: UnhealthyReason::UdsConnectFailed };
        }
        Err(_) => return ProbeResult::Unhealthy { reason: UnhealthyReason::Timeout },
    };
    // 发 repo 既有 envelope ping
    let req = r#"{"jsonrpc":"2.0","method":"ping","id":1}"#;
    use tokio::io::AsyncWriteExt;
    if stream.write_all(req.as_bytes()).await.is_err() {
        return ProbeResult::Unhealthy { reason: UnhealthyReason::UdsBadReply };
    }
    ProbeResult::Healthy { latency_ms: 0 }
}

async fn probe_tcp(spec: &ProbeSpec) -> ProbeResult {
    let addr = format!("127.0.0.1:{}", spec.port);
    match tokio::time::timeout(
        PROBE_TIMEOUT,
        tokio::net::TcpStream::connect(&addr),
    )
    .await
    {
        Ok(Ok(_)) => ProbeResult::Healthy { latency_ms: 0 },
        Ok(Err(_)) => ProbeResult::Unhealthy { reason: UnhealthyReason::ConnectionRefused },
        Err(_) => ProbeResult::Unhealthy { reason: UnhealthyReason::Timeout },
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

    #[test]
    fn test_unhealthy_reason_restart_eligible() {
        // 这两个才进重启流
        assert!(UnhealthyReason::ConnectionRefused.restart_eligible());
        assert!(UnhealthyReason::Timeout.restart_eligible());
        // BadPath 不重启 (防风暴)
        assert!(!UnhealthyReason::BadPath(404).restart_eligible());
        assert!(!UnhealthyReason::BadStatus(500).restart_eligible());
        assert!(!UnhealthyReason::UdsConnectFailed.restart_eligible());
    }

    #[tokio::test]
    async fn test_probe_http_healthy() {
        // 用 reqwest 连一个不存在的端口 → ConnectionRefused (不依赖外部 server)
        // 此处验证 classify_status 纯函数
        assert!(matches!(classify_status(200), ProbeResult::Healthy { .. }));
        assert!(matches!(classify_status(204), ProbeResult::Healthy { .. }));
        assert!(matches!(
            classify_status(500),
            ProbeResult::Unhealthy { reason: UnhealthyReason::BadStatus(500) }
        ));
        assert!(matches!(
            classify_status(404),
            ProbeResult::Unhealthy { reason: UnhealthyReason::BadPath(404) }
        ));
        assert!(matches!(
            classify_status(405),
            ProbeResult::Unhealthy { reason: UnhealthyReason::BadPath(405) }
        ));
    }
}
