pub mod agents;
mod cli;
mod doctor;
mod error;
pub mod install;
pub mod models;
mod os_paths;
pub mod registry;
pub mod skills;
pub mod sources;

use std::process::ExitCode;

fn main() -> ExitCode {
    match cli::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => error::report(error),
    }
}
