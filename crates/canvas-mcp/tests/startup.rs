//! Start-up behaviour: without a working session the server must exit
//! non-zero before the MCP handshake, so the client shows it as failed.

use std::process::Command;

#[test]
fn exits_non_zero_with_login_instruction_when_session_does_not_apply() {
    let dir = std::env::temp_dir().join(format!("canvas-mcp-startup-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config.toml");
    // The keychain account is one that never exists, so there is no
    // session, and the user's real one is never read.
    std::fs::write(
        &config,
        "canvas_url = \"http://127.0.0.1:9\"\npanopto_host = \"example.hosted.panopto.com\"\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_canvas-mcp"))
        .env("CANVAS_CLI_CONFIG", &config)
        .env(
            "CANVAS_CLI_KEYCHAIN_ACCOUNT",
            "canvas-cli-test-no-such-account",
        )
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();

    std::fs::remove_dir_all(&dir).unwrap();
    assert!(!output.status.success());
    assert!(
        output.stdout.is_empty(),
        "nothing may be written to the MCP channel"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("canvas auth login"), "stderr was: {stderr}");
}

#[test]
fn exits_non_zero_with_setup_instruction_when_not_configured() {
    let output = Command::new(env!("CARGO_BIN_EXE_canvas-mcp"))
        .env("CANVAS_CLI_CONFIG", "/no/such/canvas-cli-config.toml")
        .env(
            "CANVAS_CLI_KEYCHAIN_ACCOUNT",
            "canvas-cli-test-no-such-account",
        )
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        output.stdout.is_empty(),
        "nothing may be written to the MCP channel"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("canvas setup"));
}
