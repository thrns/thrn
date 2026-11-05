use std::io;

#[derive(Debug, thiserror::Error)]
pub enum ThrnError {
    #[error("could not write command output: {0}")]
    Output(#[from] io::Error),
}

pub type Result<T> = std::result::Result<T, ThrnError>;
