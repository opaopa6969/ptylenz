mod block;
mod claude_feeder;
mod pty;
mod tui_app;

use anyhow::Result;
use std::env;

fn main() -> Result<()> {
    let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());

    // TODO: parse CLI args (--shell, --no-integrate, --export, etc.)

    let app = tui_app::App::new(&shell)?;
    app.run()
}
