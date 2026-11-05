//! Without setup, commands explain how to configure; `canvas setup` with
//! flags saves the addresses without prompting.

#![allow(clippy::unwrap_used)]

use std::process::Command;

fn canvas(config: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_canvas"))
        .args(args)
        .env("CANVAS_CLI_CONFIG", config)
        .env(
            "CANVAS_CLI_KEYCHAIN_ACCOUNT",
            "canvas-cli-test-no-such-account",
        )
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn unconfigured_commands_point_to_setup_and_setup_saves() {
    let dir = std::env::temp_dir().join(format!("canvas-cli-setup-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config.toml");

    for args in [
        &["courses"][..],
        &["assignments"],
        &["recap", "list", "ABC1020"],
    ] {
        let out = canvas(&config, args);
        assert!(!out.status.success(), "{args:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("canvas setup"),
            "{args:?}"
        );
    }

    // No terminal and no flags: it must not hang waiting for input.
    let out = canvas(&config, &["setup"]);
    assert!(!out.status.success());

    let out = canvas(
        &config,
        &[
            "setup",
            "--canvas-url",
            "canvas.example.edu/courses/1",
            "--recap-url",
            "https://example.hosted.panopto.com/Panopto/Pages/Viewer.aspx?id=1",
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let saved = std::fs::read_to_string(&config).unwrap();
    assert!(
        saved.contains("canvas_url = \"https://canvas.example.edu\""),
        "{saved}"
    );
    assert!(
        saved.contains("panopto_host = \"example.hosted.panopto.com\""),
        "{saved}"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}
