use crate::rpc::schema::{RpcRequest, RpcResponse};
use anyhow::{anyhow, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub struct RpcClient {
    stream: UnixStream,
}

impl RpcClient {
    pub async fn connect(socket_path: &str) -> Result<Self> {
        let stream = UnixStream::connect(socket_path)
            .await
            .map_err(|e| anyhow!("daemon 未运行, 用 `fusion-sv daemon` 启动: {e}"))?;
        Ok(Self { stream })
    }

    pub async fn call(&mut self, req: RpcRequest) -> Result<RpcResponse> {
        let mut s = serde_json::to_string(&req)?;
        s.push('\n');
        self.stream.write_all(s.as_bytes()).await?;
        let mut reader = BufReader::new(&mut self.stream);
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        let resp: RpcResponse = serde_json::from_str(line.trim())?;
        Ok(resp)
    }

    pub async fn ping(&mut self) -> Result<bool> {
        let req = RpcRequest { jsonrpc: "2.0".into(), method: "ping".into(), params: Value::Null, id: 1 };
        let resp = self.call(req).await?;
        Ok(resp.result.map(|v| v == "pong").unwrap_or(false))
    }
}
