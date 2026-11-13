//! `beacon serve`: runs the server (`spec/config.md`). The server crate isn't written yet, so
//! [`run`] stands in for `beacon_server::run` and says so; wiring it is one line here.

use crate::args::Args;
use crate::{Done, Fail};
use beacon_core::Problem;
use std::path::Path;

/// `beacon serve [--config beacon.toml]`
pub fn serve(raw: Vec<String>) -> Done {
    let args = Args::parse("serve", raw, &["config"], &[])?;
    args.at_most("serve", 0)?;
    run(args.value("config").map(Path::new))
}

/// Stands in for `beacon_server::run(config)` until crates/server has it.
fn run(config: Option<&Path>) -> Done {
    let _ = config;
    Err(Fail::refused(vec![Problem::new(
        "beacon serve",
        "the server is not built yet",
        "crates/server will provide it; until then `beacon check`, `gen`, `publish`, `send`, `snapshot` and `silence` work against a running Beacon",
    )]))
}
