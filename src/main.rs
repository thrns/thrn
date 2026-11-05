mod cli;
mod doctor;
mod error;
pub mod models;
pub mod registry;

use std::process::ExitCode;

fn main() -> ExitCode {
    match cli::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => error::report(error),
    }
}
