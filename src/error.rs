use std::{io, io::Write, process::ExitCode};

#[derive(Debug, thiserror::Error)]
pub enum ThrnError {
    #[error(transparent)]
    Cli(#[from] clap::Error),
    #[error("could not write command output: {0}")]
    Output(#[from] io::Error),
}

pub type Result<T> = std::result::Result<T, ThrnError>;

pub fn report(error: ThrnError) -> ExitCode {
    match error {
        ThrnError::Cli(error) => {
            let exit_code = match u8::try_from(error.exit_code()) {
                Ok(code) => ExitCode::from(code),
                Err(_) => ExitCode::FAILURE,
            };
            match error.print() {
                Ok(()) => exit_code,
                Err(print_error) => write_diagnostic(&format!(
                    "thrn: could not write command-line response: {print_error}"
                )),
            }
        }
        error => write_diagnostic(&format!("thrn: {error}")),
    }
}

fn write_diagnostic(message: &str) -> ExitCode {
    let mut stderr = io::stderr().lock();
    let _ = writeln!(stderr, "{message}");
    ExitCode::FAILURE
}
