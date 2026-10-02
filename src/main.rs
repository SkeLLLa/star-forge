//! CLI parsing and dispatch for `stfgd` (daemon + admin commands, plus a `get` that
//! delegates to the shared client); client-side commands are synchronous, the daemon is
//! not started here directly (see `daemon::run`).

mod blocking;
mod cache;
mod client;
mod config;
mod daemon;
mod duration;
mod extract;
mod ipc;
mod provider;

use std::time::Duration;

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(cmd) = args.next() else {
        eprintln!("usage: stfgd <get|daemon|status|stop|reload> ...");
        std::process::exit(1);
    };

    match cmd.as_str() {
        "get" => cmd_get(args),
        "daemon" => daemon::run(),
        "status" => cmd_status(),
        "stop" => cmd_admin("stop"),
        "reload" => cmd_admin("reload"),
        other => {
            eprintln!("stfgd: unknown command {other:?}");
            std::process::exit(1);
        }
    }
}

/// The hot path: always exits 0, one line per requested badge, never writes to stderr.
/// Same implementation as `stfg` (see `client.rs`).
fn cmd_get(args: impl Iterator<Item = String>) {
    let (badges, cwd) = client::parse_get_args(args);
    let values = client::client_get(&badges, cwd.as_deref());
    client::print_values(&values);
}

fn cmd_status() {
    match ipc::client_admin("status", Duration::from_secs(1)) {
        Some(resp) => println!(
            "{}",
            resp.values
                .get("status")
                .map_or("(no badges queried yet)", String::as_str)
        ),
        None => eprintln!("stfgd: daemon not running"),
    }
}

fn cmd_admin(cmd: &str) {
    match ipc::client_admin(cmd, Duration::from_secs(1)) {
        Some(_) => println!("stfgd: {cmd} ok"),
        None => eprintln!("stfgd: daemon not running"),
    }
}
