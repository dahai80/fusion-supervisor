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
