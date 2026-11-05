mod cli;
mod config;
mod costs;
mod gcp;
mod mcp;
mod modules;
mod platform;
mod tui;
mod update;
mod util;

use std::process::ExitCode;

use clap::{CommandFactory, FromArgMatches};

fn main() -> ExitCode {
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
