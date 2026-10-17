//! `sshub map`: the jump-host map as text for other tools.

use anyhow::Result;

use super::context::CliContext;
use super::parse::{take_flag, usage};

pub fn run(ctx: &CliContext, args: &[String]) -> Result<i32> {
    let mut rest = args.to_vec();
    let dot = take_flag(&mut rest, "--dot");
    if let Some(extra) = rest.first() {
        usage(&format!("map: unexpected argument {extra:?}"));
    }
    if !dot {
        usage("map needs --dot (Graphviz output)");
    }
    let hosts = crate::map::collect_hosts(&ctx.store, &ctx.hosts)?;
    let map = crate::map::Map::build(hosts);
    // `sshub map --dot | head -1` closes the pipe early: that is not an error.
    use std::io::Write as _;
    match std::io::stdout()
        .lock()
        .write_all(crate::map::dot::to_dot(&map).as_bytes())
    {
        Err(e) if e.kind() != std::io::ErrorKind::BrokenPipe => Err(e.into()),
        _ => Ok(0),
    }
}
