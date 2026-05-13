use std::env;
use std::process::ExitCode;

use plugin_kitty::lifecycle;

fn main() -> ExitCode {
    match env::args().nth(1).as_deref() {
        Some("snapshot") => match lifecycle::snapshot() {
            Ok(n) => {
                println!("plugin-kitty snapshot: captured {n} pane(s)");
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("plugin-kitty snapshot: {err:#}");
                ExitCode::from(1)
            }
        },
        Some("restore") => match lifecycle::restore() {
            Ok(n) => {
                println!("plugin-kitty restore: launched {n} pane(s)");
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("plugin-kitty restore: {err:#}");
                ExitCode::from(1)
            }
        },
        None | Some("daemon") | Some("run") => match lifecycle::daemon_run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("plugin-kitty daemon: {err:#}");
                ExitCode::from(1)
            }
        },
        Some("settings") => {
            println!("plugin-kitty: settings (placeholder)");
            ExitCode::SUCCESS
        }
        Some(action) => {
            eprintln!("Unknown action: {action}");
            ExitCode::from(1)
        }
    }
}
