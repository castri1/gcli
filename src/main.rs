mod apps;
mod builds;
mod cli;
mod config;
mod costs;
mod gcp;
mod logs;
mod mcp;
mod modules;
mod platform;
mod tui;
mod update;
mod util;

use std::process::ExitCode;

use clap::{CommandFactory, FromArgMatches};

/// What `println!` panics with when the reader went away (`gcli apps | head`): the end of the
/// output, not a crash. The exit code stays what a shell reports for it, never a check's 0.
fn is_closed_pipe(panic_message: &str) -> bool {
    panic_message.starts_with("failed printing to stdout") && panic_message.to_ascii_lowercase().contains("pipe")
}

const CLOSED_PIPE_EXIT: i32 = 141;

fn end_quietly_on_a_closed_pipe() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info.payload();
        let message = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
            .unwrap_or("");
        if is_closed_pipe(message) {
            std::process::exit(CLOSED_PIPE_EXIT);
        }
        previous(info);
    }));
}

fn main() -> ExitCode {
    end_quietly_on_a_closed_pipe();
    update::cleanup();
    // Usage and help show the name the binary was invoked as.
    let matches = cli::Cli::command().bin_name(util::program()).get_matches();
    let args = cli::Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    let code = match cli::run(args) {
        Ok(code) => code,
        Err(e) => {
            // Errors can quote Google Cloud responses or a config file; keep them from driving the terminal.
            eprintln!("error: {}", util::sanitize(&format!("{e:#}")));
            if e.downcast_ref::<gcp::GcpError>().is_some_and(gcp::GcpError::is_login_required) {
                eprintln!("hint: run `{} login` (or press L in the UI)", util::program());
            }
            ExitCode::FAILURE
        }
    };
    // Which credentials were used, for when a sign-in does not seem to take (never the token).
    if std::env::var_os("GCLI_DEBUG").is_some()
        && let Some(how) = gcp::auth::shared().describe()
    {
        eprintln!("gcli: credentials: {how}");
    }
    code
}

#[cfg(test)]
mod tests {
    use super::is_closed_pipe;

    #[test]
    fn only_a_closed_pipe_ends_quietly() {
        assert!(is_closed_pipe("failed printing to stdout: Broken pipe (os error 32)"));
        assert!(is_closed_pipe("failed printing to stdout: The pipe is being closed. (os error 232)"));
        assert!(!is_closed_pipe("failed printing to stdout: No space left on device (os error 28)"));
        assert!(!is_closed_pipe("index out of bounds"));
    }
}
