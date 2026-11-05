//! `canvas`: command-line access to Canvas and Panopto ReCap.
//!
//! This binary is a thin layer: it parses arguments, loads the config,
//! calls the library crates and prints text or `--json`. All logic lives
//! in the libraries so the MCP server behaves the same way.

use clap::{Parser, Subcommand};

/// Read Canvas, download course files and ReCap lectures.
#[derive(Debug, Parser)]
#[command(name = "canvas", version, about)]
struct Cli {
    /// Print machine-readable JSON instead of text.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

/// Top-level commands.
#[derive(Debug, Subcommand)]
enum Command {
    /// Show the active config file and settings.
    Config,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let path = canvas_core::Config::default_path()?;
    let config = canvas_core::Config::load(&path)?;
    match cli.command {
        Command::Config => {
            if cli.json {
                let out = serde_json::json!({ "path": path, "config": config });
                println!("{out}");
            } else {
                println!("config         {}", path.display());
                println!("canvas_url     {}", config.canvas_url);
                println!("panopto_host   {}", config.panopto_host);
                println!("download_root  {}", config.download_root.display());
            }
        }
    }
    Ok(())
}
