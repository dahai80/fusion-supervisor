#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
    #[serde(default)]
    pub id: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
    pub id: i64,
}

#[derive(Debug, Clone, Copy)]
pub enum Method {
    Status,
    Up,
    Down,
    Restart,
    Top,
    Ping,
    Shutdown,
}

pub fn parse_method(s: &str) -> Option<Method> {
    match s.to_lowercase().as_str() {
        "status" => Some(Method::Status),
        "up" => Some(Method::Up),
        "down" => Some(Method::Down),
        "restart" => Some(Method::Restart),
        "top" => Some(Method::Top),
        "ping" => Some(Method::Ping),
        "shutdown" => Some(Method::Shutdown),
        _ => None,
    }
}

pub fn make_result(result: Value, id: i64) -> RpcResponse {
    RpcResponse { jsonrpc: "2.0".into(), result: Some(result), error: None, id }
}

pub fn make_error(code: i64, message: &str, id: i64) -> RpcResponse {
    RpcResponse { jsonrpc: "2.0".into(), result: None, error: Some(RpcError { code, message: message.into() }), id }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_method() {
        assert!(matches!(parse_method("status"), Some(Method::Status)));
        assert!(matches!(parse_method("up"), Some(Method::Up)));
        assert!(matches!(parse_method("ping"), Some(Method::Ping)));
        assert!(matches!(parse_method("SHUTDOWN"), Some(Method::Shutdown)));
    }

    #[test]
    fn test_make_result_response() {
        let r = make_result(json!("pong"), 42);
        assert_eq!(r.jsonrpc, "2.0");
        assert_eq!(r.result, Some(json!("pong")));
        assert!(r.error.is_none());
        assert_eq!(r.id, 42);
    }

    #[test]
    fn test_make_error_response() {
        let r = make_error(-32601, "method not found", 7);
        assert!(r.result.is_none());
        assert_eq!(r.error.unwrap().code, -32601);
        assert_eq!(r.id, 7);
    }

    #[test]
    fn test_envelope_roundtrip_serde() {
        let req = json!({"jsonrpc":"2.0","method":"ping","params":null,"id":1});
        let parsed: RpcRequest = serde_json::from_value(req).unwrap();
        assert_eq!(parsed.method, "ping");
        assert_eq!(parsed.id, 1);
        let resp = make_result(json!("pong"), 1);
        let s = serde_json::to_string(&resp).unwrap();
        assert!(s.contains("\"result\":\"pong\""));
        assert!(s.contains("\"id\":1"));
    }
}
