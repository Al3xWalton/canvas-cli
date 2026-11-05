//! The MCP tool definitions.
//!
//! Each tool is a thin wrapper over a `canvas-api` call. Results are
//! returned as structured JSON. Failures become MCP tool errors (not
//! protocol errors) so the agent sees the message, including the
//! `canvas auth login` instruction when the session has expired.

use canvas_api::{Client, User};
use canvas_core::Config;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{schemars, tool, tool_handler, tool_router};
use serde::Deserialize;

/// The agent guide, sent as `instructions` and by the `usage` tool.
pub const GUIDE: &str = include_str!("guide.md");

/// The Canvas CLI MCP server.
#[derive(Debug, Clone)]
pub struct CanvasServer {
    config: Config,
    client: Client,
    user: User,
}

/// Arguments for `list_assignments`.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct AssignmentsArgs {
    /// How far ahead to look, e.g. `7d`, `48h`, `2w`. Defaults to `7d`.
    #[serde(default)]
    pub within: Option<String>,
    /// Only this course id (from `list_courses`).
    #[serde(default)]
    pub course_id: Option<u64>,
}

/// Turns any serialisable value into a structured tool result.
fn ok(value: impl serde::Serialize) -> CallToolResult {
    match serde_json::to_value(value) {
        Ok(v) => CallToolResult::structured(v),
        Err(e) => fail(e),
    }
}

/// Turns an error into a tool error the agent can read.
fn fail(e: impl std::fmt::Display) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(e.to_string())])
}

#[tool_router]
impl CanvasServer {
    /// Builds the server for an already-verified session.
    pub fn new(config: Config, client: Client, user: User) -> Self {
        Self {
            config,
            client,
            user,
        }
    }

    /// Reports who is signed in, re-checking the session with Canvas.
    #[tool(
        description = "Check the Canvas session and return the signed-in user. Call first in a long job."
    )]
    async fn auth_status(&self) -> CallToolResult {
        match self.client.current_user().await {
            Ok(user) => ok(serde_json::json!({
                "signed_in": true,
                "user": user,
                "canvas_url": self.config.canvas_url,
            })),
            Err(e) => fail(e),
        }
    }

    /// Lists active courses.
    #[tool(description = "List the user's active Canvas courses (id, course_code, name, term).")]
    async fn list_courses(&self) -> CallToolResult {
        match self.client.courses().await {
            Ok(courses) => ok(courses),
            Err(e) => fail(e),
        }
    }

    /// Lists assignments due soon.
    #[tool(
        description = "List assignments due within a window (default 7d), soonest first, with due_at (RFC 3339 UTC) and submitted."
    )]
    async fn list_assignments(
        &self,
        Parameters(args): Parameters<AssignmentsArgs>,
    ) -> CallToolResult {
        let within = match canvas_api::parse_window(args.within.as_deref().unwrap_or("7d")) {
            Ok(w) => w,
            Err(e) => return fail(e),
        };
        match canvas_api::upcoming_assignments(
            &self.client,
            within,
            chrono::Utc::now(),
            args.course_id,
        )
        .await
        {
            Ok(list) => ok(list),
            Err(e) => fail(e),
        }
    }

    /// Returns the agent guide.
    #[tool(
        description = "Return the guide to these tools: what each does, recipes and error handling."
    )]
    async fn usage(&self) -> CallToolResult {
        CallToolResult::success(vec![ContentBlock::text(GUIDE)])
    }
}

#[tool_handler]
impl rmcp::ServerHandler for CanvasServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("canvas-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(format!("{GUIDE}\nSigned in as {}.", self.user.name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_session_reaches_the_agent_with_the_fix() {
        let result = fail(canvas_core::Error::SessionExpired);
        assert_eq!(result.is_error, Some(true));
        let text = format!("{:?}", result.content);
        assert!(text.contains("canvas auth login"));
    }

    #[test]
    fn guide_names_every_tool() {
        for tool in ["auth_status", "list_courses", "list_assignments"] {
            assert!(GUIDE.contains(tool), "{tool} missing from guide");
        }
    }
}
