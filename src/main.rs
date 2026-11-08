//! herdr-superagent: a herdr plugin with one superagent session that routes
//! your requests into tickets, runs workers in herdr panes and lets you review.

mod cli;
// The executor-facing engine API (assignment lifecycle, delivery, status requests)
// is exercised by the tests now and wired to herdr panes in phase 2.
#[allow(dead_code)]
mod core;
mod daemon;
mod ensure;
// Pane and workspace fields that phase 2's observer reads.
#[allow(dead_code)]
mod herdr;
mod ipc;
mod paths;
mod profile;
mod store;

use clap::Parser;

fn main() {
    let filter =
        tracing_subscriber::EnvFilter::try_from_env("HERDR_SUPERAGENT_LOG").unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
    let cli = cli::Cli::parse();
    match cli::run(cli) {
        Ok(code) => std::process::exit(code),
        Err(message) => {
            eprintln!("herdr-superagent: {message}");
            std::process::exit(1);
        }
    }
}
