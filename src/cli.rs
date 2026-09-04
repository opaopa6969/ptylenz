use std::env;
use std::fmt;

const DEFAULT_SHELL: &str = "/bin/bash";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunConfig {
    pub shell_path: String,
    pub shell_integration: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Run(RunConfig),
    Help,
    Version,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    message: String,
}

impl CliError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

pub fn parse_args<I>(args: I) -> Result<Command, CliError>
where
    I: IntoIterator<Item = String>,
{
    let mut iter = args.into_iter();
    let _program = iter.next();

    let mut shell_path = env::var("SHELL").unwrap_or_else(|_| DEFAULT_SHELL.to_string());
    let mut shell_integration = true;

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "--version" => return Ok(Command::Version),
            "--no-integrate" => shell_integration = false,
            "--shell" => {
                let value = iter
                    .next()
                    .ok_or_else(|| CliError::new("missing value for --shell"))?;
                if value.is_empty() {
                    return Err(CliError::new("shell path for --shell must not be empty"));
                }
                shell_path = value;
            }
            _ if arg.starts_with('-') => {
                return Err(CliError::new(format!("unknown option: {arg}")));
            }
            _ => {
                return Err(CliError::new(format!(
                    "unexpected positional argument: {arg}"
                )));
            }
        }
    }

    Ok(Command::Run(RunConfig {
        shell_path,
        shell_integration,
    }))
}

pub fn help_text() -> String {
    format!(
        "\
ptylenz {version}

Usage:
  ptylenz [OPTIONS]

Options:
  --shell <PATH>   Launch the given shell instead of $SHELL
  --no-integrate   Skip bash OSC 133 rcfile injection
  --version        Print version and exit
  -h, --help       Print help and exit
",
        version = env!("CARGO_PKG_VERSION")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_string()).collect()
    }

    #[test]
    fn parse_defaults_to_run_command() {
        let args = argv(&["ptylenz"]);
        let command = parse_args(args).expect("parse succeeds");
        match command {
            Command::Run(config) => {
                assert!(!config.shell_path.is_empty());
                assert!(config.shell_integration);
            }
            other => panic!("expected run command, got {other:?}"),
        }
    }

    #[test]
    fn parse_shell_override_and_no_integrate() {
        let args = argv(&["ptylenz", "--shell", "/bin/sh", "--no-integrate"]);
        let command = parse_args(args).expect("parse succeeds");
        assert_eq!(
            command,
            Command::Run(RunConfig {
                shell_path: "/bin/sh".to_string(),
                shell_integration: false,
            })
        );
    }

    #[test]
    fn parse_help_and_version_short_circuit() {
        assert_eq!(
            parse_args(argv(&["ptylenz", "--help"])).unwrap(),
            Command::Help
        );
        assert_eq!(
            parse_args(argv(&["ptylenz", "--version"])).unwrap(),
            Command::Version
        );
    }

    #[test]
    fn parse_rejects_missing_shell_value() {
        let error = parse_args(argv(&["ptylenz", "--shell"])).expect_err("must fail");
        assert_eq!(error.to_string(), "missing value for --shell");
    }

    #[test]
    fn parse_rejects_unknown_option() {
        let error = parse_args(argv(&["ptylenz", "--bogus"])).expect_err("must fail");
        assert_eq!(error.to_string(), "unknown option: --bogus");
    }

    #[test]
    fn parse_rejects_positional_arguments() {
        let error = parse_args(argv(&["ptylenz", "bash"])).expect_err("must fail");
        assert_eq!(error.to_string(), "unexpected positional argument: bash");
    }
}
