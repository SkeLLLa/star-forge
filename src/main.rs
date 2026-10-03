//! Entry point for `stfgd`: `daemon` runs the server (`daemon::run`); `status`/`stop`/`reload`
//! are admin clients over the socket, with LSB exit codes when the daemon isn't running (see
//! `NOT_RUNNING_*`); `get` delegates to the shared client.

mod blocking;
mod client;
mod config;
mod daemon;
mod duration;
mod extract;
mod ipc;
mod provider;
mod template;
#[cfg(test)]
mod test_support;

use std::time::Duration;

/// LSB init-script exit codes (also used by `systemctl`) for a daemon that isn't running:
/// `status` → 3, `reload` → 7; `stop` of a stopped daemon is a success (0). A daemon that's
/// there but doesn't answer is a generic failure (1).
const NOT_RUNNING_STATUS: i32 = 3;
const NOT_RUNNING_RELOAD: i32 = 7;

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
        "stop" => cmd_admin("stop", 0),
        "reload" => cmd_admin("reload", NOT_RUNNING_RELOAD),
        other => {
            eprintln!("stfgd: unknown command {other:?}");
            std::process::exit(1);
        }
    }
}

/// The hot path: always exits 0, one line per requested badge, never writes to stderr.
/// Same implementation as `stfg` (see `client.rs`).
fn cmd_get(args: impl Iterator<Item = String>) {
    client::run_cli(args);
}

fn cmd_status() {
    match ipc::client_admin("status", Duration::from_secs(1)) {
        Ok(resp) => println!(
            "{}",
            resp.values
                .get("status")
                .map_or("(no badges queried yet)", String::as_str)
        ),
        Err(e) => admin_failed(e, NOT_RUNNING_STATUS),
    }
}

/// Exits with `not_running_code` if no daemon is listening, else with 1 (generic failure,
/// e.g. a hung daemon): a `stop` that couldn't reach a live daemon must not report success.
fn admin_failed(err: ipc::AdminError, not_running_code: i32) {
    match err {
        ipc::AdminError::NotRunning => {
            eprintln!("stfgd: daemon not running");
            std::process::exit(not_running_code);
        }
        ipc::AdminError::NoResponse => {
            eprintln!("stfgd: daemon not responding");
            std::process::exit(1);
        }
    }
}

fn cmd_admin(cmd: &str, not_running_code: i32) {
    match ipc::client_admin(cmd, Duration::from_secs(1)) {
        Ok(_) => println!("stfgd: {cmd} ok"),
        Err(e) => admin_failed(e, not_running_code),
    }
}
