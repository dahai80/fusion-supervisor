use crate::rpc::schema::{RpcRequest, RpcResponse};
use anyhow::{Result, anyhow};
use serde_json::Value;

// UDS JSON-RPC client (same-host CLI face of the daemon). windows has no UnixStream —
// connecting errors immediately, directing users to the HTTP API face (the daemon HTTP port is cross-platform).
pub struct RpcClient {
    #[cfg(unix)]
    stream: tokio::net::UnixStream,
    #[cfg(not(unix))]
    _priv: (),
}

impl RpcClient {
    pub async fn connect(socket_path: &str) -> Result<Self> {
        #[cfg(unix)]
        {
            let stream = tokio::net::UnixStream::connect(socket_path)
                .await
                .map_err(|e| {
                    anyhow!("daemon not running; start it with `fusion-sv daemon`: {e}")
                })?;
            Ok(Self { stream })
        }
        #[cfg(not(unix))]
        {
            let _ = socket_path;
            Err(anyhow!(
                "uds RPC not supported on this platform; use the daemon HTTP API instead"
            ))
        }
    }

    pub async fn call(&mut self, req: RpcRequest) -> Result<RpcResponse> {
        #[cfg(unix)]
        {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
            let mut s = serde_json::to_string(&req)?;
            s.push('\n');
            self.stream.write_all(s.as_bytes()).await?;
            let mut reader = BufReader::new(&mut self.stream);
            let mut line = String::new();
            reader.read_line(&mut line).await?;
            let resp: RpcResponse = serde_json::from_str(line.trim())?;
            Ok(resp)
        }
        #[cfg(not(unix))]
        {
            let _ = req;
            Err(anyhow!(
                "uds RPC not supported on this platform; use the daemon HTTP API instead"
            ))
        }
    }

    pub async fn ping(&mut self) -> Result<bool> {
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            method: "ping".into(),
            params: Value::Null,
            id: 1,
        };
        let resp = self.call(req).await?;
        Ok(resp.result.map(|v| v == "pong").unwrap_or(false))
    }
}
