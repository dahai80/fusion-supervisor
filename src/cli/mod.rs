use crate::supervisor::StatusEntry;
use tabled::{Table, Tabled};

#[derive(Tabled)]
struct Row {
    service: String,
    state: String,
    port: u16,
    plane: String,
    // issue #112: most recent restart-class event (AutoRestart/ManualStart/ManualStop)
    last_restart: String,
}

pub fn print_status(entries: &[StatusEntry]) {
    let rows: Vec<Row> = entries
        .iter()
        .map(|e| Row {
            service: e.name.clone(),
            state: e.state.clone(),
            port: e.port,
            plane: e.plane.clone(),
            last_restart: e.last_restart.clone().unwrap_or_default(),
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

// issue #41: [SERVICE] parameter routing for up/down — Some(non-empty) → single-service RPC (params.service),
// None/empty → fleet-wide RPC. Previously main.rs `{ .. }` discarded the argument, so `down <svc>` actually
// stopped everything (fleet-kill). Pure function, testable — a regression here is an incident; main.rs must
// route through this, no direct argument discarding.
pub fn up_down_target(service: Option<String>) -> Option<String> {
    service.filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    // issue #41: the service argument must route to the single-service RPC; empty/None means fleet-wide
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
