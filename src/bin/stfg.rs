//! Tiny hot-path client: `stfg <badge>... [--cwd <path>]`. Links only std + libc (see
//! `client.rs`, shared with `stfgd get`); this binary only forwards argv to it.

#[path = "../client.rs"]
mod client;
#[cfg(test)]
#[path = "../test_support.rs"]
mod test_support;

fn main() {
    client::run_cli(std::env::args().skip(1));
}
