// PRD §4.2 API 层: ureq 同步客户端 (后台线程跑, UI 线程零阻塞)。
// 读 = GET /api/v1/boot (单端点含 tier/pin/state/port); 写 = pin/group/up/down/restart/drain/rollout。
// 单测只覆盖纯逻辑 (URL/请求构造), HTTP 走手测清单 (PRD §5.6)。

use std::time::Duration;

use serde_json::json;

use crate::state::{BootRow, Model, model_from_boot};

pub const DEFAULT_BASE: &str = "http://127.0.0.1:9100";

// 写操作 (UI 线程入队, worker 线程 ureq 执行)。
#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    Up(String),
    Down(String),
    Restart(String),
    Drain(String),
    Rollout(String),
    Pin { service: String, mode: PinMode },
    Group { group: TierKey, action: Action },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PinMode {
    Auto,
    Manual,
    Clear,
}

impl PinMode {
    pub fn as_str(self) -> &'static str {
        match self {
            PinMode::Auto => "auto",
            PinMode::Manual => "manual",
            PinMode::Clear => "clear",
        }
    }
}

// /api/v1/boot/group 的 group 参数: supervisor 侧接受 T0/T1/T2。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TierKey {
    T0,
    T1,
    T2,
}

impl TierKey {
    pub fn as_str(self) -> &'static str {
        match self {
            TierKey::T0 => "T0",
            TierKey::T1 => "T1",
            TierKey::T2 => "T2",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    Up,
    Down,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Up => "up",
            Action::Down => "down",
        }
    }
}

pub fn boot_url(base: &str) -> String {
    format!("{base}/api/v1/boot")
}

pub fn pin_url(base: &str) -> String {
    format!("{base}/api/v1/boot/pin")
}

pub fn group_url(base: &str) -> String {
    format!("{base}/api/v1/boot/group")
}

pub fn svc_op_url(base: &str, op: &str) -> String {
    format!("{base}/api/v1/{op}")
}

// 读 /api/v1/boot → Model (reachable=false 当任何 IO 错误 — 点变灰, 不白屏)。
pub fn fetch_boot(agent: &ureq::Agent, base: &str) -> Result<Model, String> {
    let body = agent
        .get(&boot_url(base))
        .timeout(Duration::from_secs(3))
        .call()
        .map_err(|e| e.to_string())?
        .into_string()
        .map_err(|e| e.to_string())?;
    let rows: Vec<BootRow> = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    Ok(model_from_boot(rows))
}

// daemon 活性探针: GET /health (handler 返回静态 {"ok":true}, 不锁核心 —
// 批量组操作占住核心锁时也能应答)。/boot 超时但 /health 通 = daemon 忙而非挂。
pub fn health_url(base: &str) -> String {
    format!("{base}/health")
}

pub fn fetch_health(agent: &ureq::Agent, base: &str) -> Result<(), String> {
    agent
        .get(&health_url(base))
        .timeout(Duration::from_secs(2))
        .call()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

// 写操作统一: 返回 Ok(json string) / Err(描述)。token 可选 (daemon 未设 FUSION_SV_TOKEN 时免)。
pub fn post_cmd(
    agent: &ureq::Agent,
    url: &str,
    body: serde_json::Value,
    token: &str,
) -> Result<String, String> {
    let mut req = agent
        .post(url)
        .timeout(Duration::from_secs(120))
        .set("Content-Type", "application/json");
    if !token.is_empty() {
        req = req.set("Authorization", &format!("Bearer {token}"));
    }
    let resp = req
        .send_string(&body.to_string())
        .map_err(|e| e.to_string())?
        .into_string()
        .map_err(|e| e.to_string())?;
    Ok(resp)
}

// Cmd → (url, body) 纯函数 (可测): UI 侧构造, worker 侧执行。
pub fn cmd_request(base: &str, cmd: &Cmd) -> (String, serde_json::Value) {
    match cmd {
        Cmd::Up(s) => (svc_op_url(base, "up"), json!({"service": s})),
        Cmd::Down(s) => (svc_op_url(base, "down"), json!({"service": s})),
        Cmd::Restart(s) => (svc_op_url(base, "restart"), json!({"service": s})),
        Cmd::Drain(s) => (svc_op_url(base, "drain"), json!({"service": s})),
        Cmd::Rollout(s) => (svc_op_url(base, "rollout"), json!({"service": s})),
        Cmd::Pin { service, mode } => (
            pin_url(base),
            json!({"service": service, "mode": mode.as_str()}),
        ),
        Cmd::Group { group, action } => (
            group_url(base),
            json!({"group": group.as_str(), "action": action.as_str()}),
        ),
    }
}

pub fn new_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(2))
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_health_url_shape() {
        assert_eq!(health_url(DEFAULT_BASE), "http://127.0.0.1:9100/health");
    }
}
