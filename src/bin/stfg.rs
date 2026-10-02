//! Tiny hot-path client: `stfg <badge>... [--cwd <path>]`. Links only std + libc (no
//! `tokio`/`serde`/`serde_json`/`regex`/`reqwest`/`toml`) so the prompt hot path pays for as
//! little startup/link cost as possible. `client.rs` (shared with `stfgd get`, see
//! `main.rs`) holds the one implementation; this file is only CLI parsing + dispatch.

#[path = "../client.rs"]
mod client;

fn main() {
    let (badges, cwd) = client::parse_get_args(std::env::args().skip(1));
    let values = client::client_get(&badges, cwd.as_deref());
    client::print_values(&values);
}
