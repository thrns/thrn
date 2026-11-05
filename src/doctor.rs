use std::io::{self, Write};

use crate::error::Result;

pub fn run() -> Result<()> {
    let output = format!(
        "THRN doctor\nVersion: {}\nPlatform: OS={}, architecture={} (reported platform constants; not a full target triple)\nDiagnostics performed: build metadata only (no filesystem or network checks)\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
    );

    let mut stdout = io::stdout().lock();
    stdout.write_all(output.as_bytes())?;
    stdout.flush()?;
    Ok(())
}
