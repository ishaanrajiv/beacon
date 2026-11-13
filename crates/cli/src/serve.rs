//! `beacon serve`: runs the server (`spec/config.md`) until the process is stopped.

use crate::args::Args;
use crate::{Done, Fail};
use std::path::Path;

/// `beacon serve [--config beacon.toml]`
pub fn serve(raw: Vec<String>) -> Done {
    let args = Args::parse("serve", raw, &["config"], &[])?;
    args.at_most("serve", 0)?;
    let config = beacon_core::config::load(args.value("config").map(Path::new)).map_err(Fail::refused)?;
    beacon_server::run(config).map_err(Fail::refused)
}
