use std::process::Command;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "xtask", about = "Build and packaging automation for starweld")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Cross-compile static release binaries for Linux + Windows.
    Dist {
        #[arg(long, default_value = "x86_64-unknown-linux-musl")]
        linux_target: String,
        #[arg(long, default_value = "x86_64-pc-windows-gnu")]
        windows_target: String,
        /// Use `cross` instead of `cargo` to drive cross compilation.
        #[arg(long)]
        cross: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Dist {
            linux_target,
            windows_target,
            cross,
        } => dist(&linux_target, &windows_target, cross),
    }
}

fn dist(linux_target: &str, windows_target: &str, use_cross: bool) -> Result<()> {
    build(linux_target, use_cross)?;
    build(windows_target, use_cross)?;
    let linux_path = format!("target/{linux_target}/release/starweld");
    let windows_path = format!("target/{windows_target}/release/starweld.exe");
    println!("Artifacts:");
    print_artifact(&linux_path);
    print_artifact(&windows_path);
    Ok(())
}

fn print_artifact(path: &str) {
    let size = std::fs::metadata(path)
        .map(|m| format!("{} bytes", m.len()))
        .unwrap_or_else(|_| "missing".to_string());
    println!("  {path} ({size})");
}

fn build(target: &str, use_cross: bool) -> Result<()> {
    let cmd_name = if use_cross { "cross" } else { "cargo" };
    let status = Command::new(cmd_name)
        .args([
            "build",
            "--release",
            "--bin",
            "starweld",
            "--target",
            target,
        ])
        .status()
        .with_context(|| format!("failed to invoke {cmd_name}"))?;
    if !status.success() {
        return Err(anyhow!("{cmd_name} build for {target} failed"));
    }
    Ok(())
}
