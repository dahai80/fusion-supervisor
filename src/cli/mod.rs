use crate::supervisor::StatusEntry;
use tabled::{Table, Tabled};

#[derive(Tabled)]
struct Row {
    service: String,
    state: String,
    port: u16,
    plane: String,
}

pub fn print_status(entries: &[StatusEntry]) {
    let rows: Vec<Row> = entries
        .iter()
        .map(|e| Row {
            service: e.name.clone(),
            state: e.state.clone(),
            port: e.port,
            plane: e.plane.clone(),
        })
        .collect();
    let table = Table::new(rows);
    println!("{table}");
}

pub fn print_pong(ok: bool) {
    if ok {
        println!("pong (daemon alive)");
    } else {
        println!("daemon no pong");
    }
}

pub fn print_ok(msg: &str) {
    println!("{msg}");
}

pub fn print_error(msg: &str) {
    eprintln!("error: {msg}");
}

// issue #41: up/down 的 [SERVICE] 参数路由 — Some(非空) → 单服务 RPC (params.service),
// None/空串 → 全量 RPC。此前 main.rs `{ .. }` 丢弃参数, `down <svc>` 实际全停 (fleet-kill)。
// 纯函数可测, 回归即事故; main.rs 必须经此路由, 不允许再直接丢参。
pub fn up_down_target(service: Option<String>) -> Option<String> {
    service.filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    // issue #41: service 参数必须路由到单服务 RPC; 空串/None 归全量
    #[test]
    fn test_up_down_target_routes_service() {
        assert_eq!(
            up_down_target(Some("fusion-finance".into())),
            Some("fusion-finance".into())
        );
        assert_eq!(up_down_target(Some(String::new())), None);
        assert_eq!(up_down_target(None), None);
    }
}
