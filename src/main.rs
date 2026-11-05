mod cli;
mod doctor;
mod error;
mod install;
pub mod models;
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
