//! herdr-superagent: a herdr plugin with one superagent session that routes
//! your requests into tickets, runs workers in herdr panes and lets you review.

mod brief;
mod calibration;
mod cli;
mod cockpit;
// The executor-facing engine API (assignment lifecycle, delivery, status requests)
// is exercised by the tests now and wired to herdr panes in phase 2.
#[allow(dead_code)]
mod core;
mod daemon;
mod dashboard;
mod embed;
mod ensure;
mod envpath;
mod executor;
mod gitbase;
mod guard;
// Pane and workspace fields that phase 2's observer reads.
#[allow(dead_code)]
mod herdr;
mod importer;
mod ipc;
mod logging;
mod memory;
mod notes;
mod notify;
mod packs;
mod paths;
mod profile;
mod session;
mod sources;
mod staterepo;
mod store;

use clap::Parser;

fn main() {
    let cli = cli::Cli::parse();
    let profile = cli.profile.clone().unwrap_or_else(|| paths::DEFAULT_PROFILE.to_string());
    let debug = logging::debug_enabled(cli.debug);
    if debug {
        // Children (the daemon pane, actions) inherit the mode.
        std::env::set_var("HERDR_SUPERAGENT_DEBUG", "1");
    }
    let process = match cli.command {
        cli::Command::Daemon => "daemon",
        cli::Command::Ensure | cli::Command::Start(_) | cli::Command::Action => "ensure",
        cli::Command::Dashboard { .. } => "dashboard",
        cli::Command::Guard { .. } => "guard",
        _ => "cli",
    };
    if paths::validate_profile(&profile).is_ok() {
        let paths = paths::Paths::for_profile(&profile);
        let _ = paths.ensure_dirs();
        logging::rotate(&logging::log_file(&paths, process), 20 * 1024 * 1024);
        logging::init(&paths, process, debug);
    }
    match cli::run(cli) {
        Ok(code) => std::process::exit(code),
        Err(message) => {
            eprintln!("herdr-superagent: {message}");
            std::process::exit(1);
        }
    }
}
