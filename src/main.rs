use std::env;
use std::process::ExitCode;

fn main() -> ExitCode {
    match env::args().nth(1).as_deref() {
        None | Some("run") => {
            println!("plugin-kitty: lifecycle daemon (placeholder)");
            ExitCode::SUCCESS
        }
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
