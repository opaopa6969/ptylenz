mod block;
mod claude_feeder;
mod cli;
mod pty;
mod tui_app;

use anyhow::Result;
use std::env;
use std::process;

use cli::Command;

fn main() -> Result<()> {
    let command = match cli::parse_args(env::args()) {
        Ok(command) => command,
        Err(error) => {
            eprintln!("ptylenz: {error}\n");
            eprintln!("{}", cli::help_text());
            process::exit(2);
        }
    };

    match command {
        Command::Help => {
            print!("{}", cli::help_text());
            Ok(())
        }
        Command::Version => {
            println!("ptylenz {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Command::Run(config) => {
            let app = tui_app::App::new(config)?;
            app.run()
        }
    }
}
