mod cli;
mod doctor;
mod error;

use std::process::ExitCode;

fn main() -> ExitCode {
    match cli::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("thrn: {error}");
            ExitCode::FAILURE
        }
    }
}
