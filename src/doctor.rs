use std::io::{self, Write};

use crate::agents::inspect_host_agent_binaries;
use crate::error::Result;

pub fn run() -> Result<()> {
    let mut output = format!(
        "THRN doctor\nVersion: {}\nPlatform: OS={}, architecture={} (reported platform constants; not a full target triple)\nDiagnostics performed: build metadata and read-only PATH executable inspection (no agent configuration or skill paths checked; no network requests)\nAgent CLI executables (PATH only; informational, not target selection):\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
    );
    for capability in inspect_host_agent_binaries() {
        let status = if capability.executable.is_some() {
            "present"
        } else {
            "not found"
        };
        output.push_str(&format!("{}: {status}\n", capability.agent.as_str()));
    }

    let mut stdout = io::stdout().lock();
    stdout.write_all(output.as_bytes())?;
    stdout.flush()?;
    Ok(())
}
