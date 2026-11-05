//! `canvas-mcp`: an MCP server that gives agents Canvas CLI's tools.
//!
//! Like the AWS MCP server, it runs on the session that `canvas auth
//! login` stored and never signs in itself. If that session is missing or
//! expired at start-up it exits non-zero, so the agent's client shows the
//! server as failed; the fix is `canvas auth login` and a reconnect.
//!
//! Status: scaffold. The stdio server lands in issue #4.

use clap::Parser;

/// Serve Canvas CLI's tools to an MCP client over stdio.
#[derive(Debug, Parser)]
#[command(name = "canvas-mcp", version, about)]
struct Args {}

fn main() -> anyhow::Result<()> {
    let _args = Args::parse();
    anyhow::bail!("canvas-mcp is not implemented yet (issue #4)")
}
