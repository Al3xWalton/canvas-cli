//! `canvas-mcp`: an MCP server that gives agents Canvas CLI's tools.
//!
//! It works like the AWS MCP server: add it to any MCP client once, and it
//! runs on the session that `canvas auth login` stored in the keychain. It
//! never signs in or opens a browser itself.
//!
//! - **Start-up check.** Before speaking MCP it calls `/api/v1/users/self`.
//!   If the session is missing or expired it prints the login instruction
//!   to stderr and exits non-zero, so the client shows the server as
//!   failed. The fix is `canvas auth login`, then a reconnect.
//! - **Mid-session expiry.** Tools return an MCP tool error carrying the
//!   same instruction, never partial data.
//! - **Same logic as the CLI.** Every tool calls the same `canvas-api`
//!   function the matching CLI command does.
//!
//! The agent guide (`guide.md`) is sent as the server's `instructions` and
//! is also returned by the `usage` tool.

mod server;

use clap::Parser;
use rmcp::ServiceExt;

/// Serve Canvas CLI's tools to an MCP client over stdio.
#[derive(Debug, Parser)]
#[command(name = "canvas-mcp", version, about)]
struct Args {}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let _args = Args::parse();
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("canvas-mcp: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> anyhow::Result<()> {
    let config = canvas_core::Config::load(&canvas_core::Config::default_path()?)?;
    let session = canvas_auth::load()?;
    let client = canvas_api::Client::new(&config, &session)?;
    // Fail before the handshake so the client marks the server as failed.
    let user = client.current_user().await?;
    // ReCap is optional: Canvas tools still work without Panopto cookies.
    // Keep why ReCap is unavailable, so tools can say so accurately.
    let recap = panopto::Panopto::new(&config, &session)
        .map(|r| r.with_persistence(true))
        .map_err(|e| match e {
            canvas_core::Error::SessionExpired => server::RecapUnavailable::SignedOut,
            other => server::RecapUnavailable::Misconfigured(other.to_string()),
        });
    let service = server::CanvasServer::new(config, client, recap, user)
        .serve(rmcp::transport::stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}
