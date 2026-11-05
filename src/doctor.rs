use std::io::{self, Write};

use crate::error::Result;

pub fn run() -> Result<()> {
    let output = format!(
        "THRN doctor\nVersion: {}\nBuild target: {}-{}\nDiagnostics performed: build metadata only (no filesystem or network checks)\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
    );

    io::stdout().lock().write_all(output.as_bytes())?;
    Ok(())
}
