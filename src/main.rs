mod cli;
mod doctor;
mod error;
mod install;
pub mod models;
pub mod registry;
pub mod skills;

use std::process::ExitCode;

fn main() -> ExitCode {
    match cli::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => error::report(error),
    }
}
