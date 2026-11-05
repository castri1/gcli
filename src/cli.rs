//! Non-interactive commands, for scripts, CI and AI agents. `--json` prints machine-readable
//! output.

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};

use crate::config::{self, Config, EnvChange, Environment, Loaded};
use crate::gcp::{Gcp, gcloud, run};
use crate::tui::picker::{self, Item, Picker};
use crate::tui::theme;
use crate::util::{display_path, ellipsize, fmt_ago, program};

#[derive(Parser)]
#[command(name = "gcli", version, about = "Explore and operate Google Cloud from your terminal")]
pub struct Cli {
    /// Config file (default: ~/.gcli/config.yaml, then ./gcli.yaml)
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Open the interactive UI (the default)
    Ui { env: Option<String> },
    /// Set the current environment, used when a command names none
    #[command(alias = "switch")]
    Use { env: Option<String> },
    /// Add an environment to the config, or change one (default: the active gcloud project and region)
    Add {
        /// Its name (default: the project id)
        name: Option<String>,
        /// Project id
        #[arg(long)]
        project: Option<String>,
        /// Region (Cloud Run's); empty means every region
        #[arg(long)]
        region: Option<String>,
        /// Changes ask for the name to be typed (`--production false` to turn it off)
        #[arg(long, value_name = "BOOL", num_args = 0..=1, default_missing_value = "true")]
        production: Option<bool>,
    },
    /// List environments
    Envs {
        #[arg(long)]
        json: bool,
    },
    /// List an environment's apps (Cloud Run services)
    Apps {
        env: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Sign in to Google Cloud (runs `gcloud auth login --update-adc`)
    Login { env: Option<String> },
    /// MCP server for Claude and other AI agents (stdio): reads Google Cloud, never changes it.
    /// Add it with: claude mcp add gcli -- gcli mcp
    Mcp(crate::mcp::Args),
    /// Update gcli to the latest release (through npm when installed with npm)
    Update {
        /// Only tell whether a newer version exists
        #[arg(long)]
        check: bool,
        /// Install this version instead of the latest (e.g. 0.2.0)
        #[arg(long, value_name = "VERSION")]
        to: Option<String>,
    },
    /// `<env>` alone opens the UI there
    #[command(external_subcommand)]
    Env(Vec<String>),
}

// ---- output helpers ----------------------------------------------------------------

pub(crate) fn color_enabled() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

pub(crate) fn paint(text: &str, code: &str) -> String {
    if color_enabled() { format!("\u{1b}[{code}m{text}\u{1b}[0m") } else { text.to_string() }
}

pub(crate) fn print_json(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

pub(crate) fn confirm(question: &str) -> bool {
    print!("{question} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    let _ = std::io::stdin().lock().read_line(&mut answer);
    matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
}

pub(crate) fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

fn env_color(production: bool) -> &'static str {
    if production { "31;1" } else { "32;1" }
}

// ---- context -----------------------------------------------------------------------

pub(crate) fn current_env(config: &Config) -> Result<String> {
    config.default_env().map(str::to_string).ok_or_else(|| {
        anyhow!("no environments: create ~/.gcli/config.yaml (see {} add), or set a project with `gcloud config set project ID`", program())
    })
}

/// The environment named, or the current one.
fn env_or_current(config: &Config, env: Option<String>) -> Result<String> {
    match env {
        Some(name) => config.env(&name).map(|_| name),
        None => current_env(config),
    }
}

/// A client for an environment whose credentials work; in a terminal, offers a sign-in when
/// they do not.
pub(crate) fn gcp_for(config: &Config, env_name: &str) -> Result<Arc<Gcp>> {
    let env = config.env(env_name)?;
    let gcp = Gcp::new(&env.project, &env.region);
    match gcp.check_credentials() {
        Err(e) if e.is_login_required() && interactive() => {
            eprintln!("{}", paint(&format!("! {e}"), "33"));
            if !confirm("Sign in now with gcloud?") {
                return Err(e.into());
            }
            gcloud::login().map_err(|e| anyhow!(e))?;
            gcp.reset_credentials();
            gcp.check_credentials()?;
        }
        other => other?,
    }
    Ok(gcp)
}

/// Picks an environment (cursor on the current one).
fn pick_env(config: &Config, title: &str) -> Result<Option<String>> {
    let current = config.default_env().unwrap_or_default().to_string();
    let names: Vec<String> = config.environments.keys().cloned().collect();
    let items: Vec<Item> = config
        .environments
        .iter()
        .map(|(name, env)| {
            Item::new(name.as_str(), env.summary())
                .color(theme::env_color(env.is_production(name)))
                .current(*name == current)
        })
        .collect();
    let cursor = config.environments.get_index_of(&current).unwrap_or(0);
    let picked = picker::pick(Picker::new(title, "select", items, false).with_cursor(cursor), theme::ACCENT)?;
    Ok(picked.map(|chosen| names[chosen[0]].clone()))
}

// ---- commands ----------------------------------------------------------------------

pub fn run(cli: Cli) -> Result<ExitCode> {
    // These need no config.
    match cli.command {
        Some(Command::Update { check, to }) => return crate::update::run(check, to),
        Some(Command::Login { env }) => return login(cli.config.as_deref(), env),
        _ => {}
    }
    let mut loaded = config::load(cli.config.as_deref())?;
    let command = cli.command.unwrap_or(Command::Ui { env: None });
    let config = &loaded.config;
    match command {
        Command::Ui { env } => {
            crate::tui::run(loaded, env.as_deref())?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Env(args) => match args.as_slice() {
            [env] if config.environments.contains_key(env) => {
                crate::tui::run(loaded, Some(env))?;
                Ok(ExitCode::SUCCESS)
            }
            _ => bail!(
                "'{}' is not a command or an environment. Environments: {}. See {} --help",
                args.join(" "),
                config.environments.keys().map(String::as_str).collect::<Vec<_>>().join(", "),
                program()
            ),
        },
        Command::Update { .. } | Command::Login { .. } => unreachable!("handled before loading the config"),
        Command::Use { env } => use_env(config, env),
        Command::Add { name, project, region, production } => add(&mut loaded, name, project, region, production),
        Command::Envs { json } => envs(&loaded, json),
        Command::Apps { env, json } => apps(config, env, json),
        Command::Mcp(args) => crate::mcp::serve(&loaded, args),
    }
}

fn use_env(config: &Config, env: Option<String>) -> Result<ExitCode> {
    let name = match env {
        Some(name) => {
            config.env(&name)?;
            name
        }
        None if interactive() => match pick_env(config, "Use environment")? {
            Some(name) => name,
            None => return Ok(ExitCode::FAILURE),
        },
        None => {
            // Not a terminal: print the list, the current one marked.
            let current = config.default_env().unwrap_or_default();
            for name in config.environments.keys() {
                println!("{} {name}", if name == current { "*" } else { " " });
            }
            return Ok(ExitCode::SUCCESS);
        }
    };
    config::set_current_env(&name)?;
    let env = config.env(&name)?;
    println!(
        "{} Using {}  {}",
        paint("✓", "32"),
        paint(&name, env_color(env.is_production(&name))),
        paint(&env.summary(), "90")
    );
    Ok(ExitCode::SUCCESS)
}

fn add(
    loaded: &mut Loaded,
    name: Option<String>,
    project: Option<String>,
    region: Option<String>,
    production: Option<bool>,
) -> Result<ExitCode> {
    let active = gcloud::active_properties();
    let name = match name.or_else(|| project.clone()).or_else(|| active.project.clone()) {
        Some(n) => n,
        None => bail!("which project? {} add [NAME] --project ID (gcloud has no project set)", program()),
    };
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("use letters, numbers, '-' and '_' for the environment name");
    }
    let exists = matches!(&loaded.source, config::Source::File(_)) && loaded.config.environments.contains_key(&name);
    let change = if exists {
        EnvChange { project, region, production }
    } else {
        // A new environment starts from the active gcloud configuration.
        let project = project.or(active.project).ok_or_else(|| anyhow!("which project? add --project ID"))?;
        EnvChange { project: Some(project), region: region.or(active.region), production }
    };
    let (path, created) = loaded.set_env(&name, &change)?;
    let env = loaded.config.env(&name)?;
    println!(
        "{} {} {}  {}",
        paint("✓", "32"),
        if created { "Added" } else { "Updated" },
        paint(&name, env_color(env.is_production(&name))),
        paint(&env.summary(), "90")
    );
    println!("{}", paint(&format!("Saved {}", display_path(&path)), "90"));
    Ok(ExitCode::SUCCESS)
}

fn env_json(name: &str, env: &Environment, current: bool) -> Value {
    json!({
        "name": name,
        "current": current,
        "project": env.project,
        "region": env.region,
        "production": env.is_production(name),
        "apps": env.apps.as_str(),
        "billing": env.billing,
    })
}

fn envs(loaded: &Loaded, json: bool) -> Result<ExitCode> {
    let config = &loaded.config;
    let current = config.default_env().unwrap_or_default();
    if json {
        let source = match &loaded.source {
            config::Source::File(p) => json!({ "file": p }),
            config::Source::Gcloud(name) => json!({ "gcloudConfiguration": name }),
        };
        let list: Vec<Value> = config.environments.iter().map(|(n, e)| env_json(n, e, n == current)).collect();
        print_json(&json!({ "source": source, "environments": list }));
        return Ok(ExitCode::SUCCESS);
    }
    match &loaded.source {
        config::Source::File(p) => println!("{}", paint(&format!("Config: {}", display_path(p)), "90")),
        config::Source::Gcloud(name) => println!(
            "{}",
            paint(
                &format!("No config file: the active gcloud configuration ({name}). Save it with: {} add", program()),
                "90"
            )
        ),
    }
    if config.environments.is_empty() {
        println!("\nNo environments: gcloud has no project set (gcloud config set project ID).");
    }
    for (name, env) in &config.environments {
        let marker = if name == current { paint(" ● current", "36") } else { String::new() };
        println!("\n{}  {}{marker}", paint(name, env_color(env.is_production(name))), paint(&env.summary(), "90"));
        let mut details = vec![format!("apps: {}", env.apps.as_str())];
        if env.is_production(name) {
            details.push("production".into());
        }
        if let Some(b) = &env.billing {
            details.push(format!("billing: {} → {}", b.account, b.export_table));
        }
        println!("  {}", paint(&details.join(" · "), "90"));
    }
    Ok(ExitCode::SUCCESS)
}

/// An app as `--json` and the MCP server give it.
pub(crate) fn service_json(s: &run::Service) -> Value {
    json!({
        "name": s.id(),
        "region": s.region(),
        "uri": s.uri,
        "latestReadyRevision": s.revision(),
        "updateTime": Some(&s.update_time).filter(|t| !t.is_empty()),
        "lastModifier": s.last_modifier,
        "minInstances": s.min_instances(),
        "maxInstances": s.max_instances(),
        "resource": s.name,
    })
}

fn apps(config: &Config, env: Option<String>, json: bool) -> Result<ExitCode> {
    let env_name = env_or_current(config, env)?;
    let gcp = gcp_for(config, &env_name)?;
    let services = run::all_services(&gcp)?;
    if json {
        print_json(&Value::Array(services.iter().map(service_json).collect()));
        return Ok(ExitCode::SUCCESS);
    }
    let id_w = services.iter().map(|s| s.id().chars().count()).max().unwrap_or(0).clamp(7, 40);
    let rev_w = services.iter().map(|s| s.revision().chars().count()).max().unwrap_or(0).clamp(8, 36);
    println!(
        "{}",
        paint(&format!("{:<id_w$}  {:<rev_w$}  {:>7}  {:>9}  BY", "SERVICE", "REVISION", "MIN/MAX", "UPDATED"), "90;1")
    );
    for s in &services {
        let scaling = format!("{}/{}", s.min_instances(), s.max_instances().map_or("-".into(), |m| m.to_string()));
        let updated = s.updated().map(fmt_ago).unwrap_or_default();
        println!(
            "{:<id_w$}  {}  {:>7}  {}  {}",
            ellipsize(s.id(), id_w),
            paint(&format!("{:<rev_w$}", ellipsize(s.revision(), rev_w)), "90"),
            scaling,
            paint(&format!("{updated:>9}"), "90"),
            paint(&s.last_modifier, "90")
        );
    }
    let env = config.env(&env_name)?;
    let count = services.len();
    println!(
        "\n{}",
        paint(&format!("{count} service{} · {env_name} · {}", if count == 1 { "" } else { "s" }, env.summary()), "90")
    );
    Ok(ExitCode::SUCCESS)
}

fn login(config_path: Option<&std::path::Path>, env: Option<String>) -> Result<ExitCode> {
    // An environment named must exist; credentials themselves are the same for all of them.
    if let Some(name) = &env {
        config::load(config_path)?.config.env(name)?;
    }
    println!("Signing in to Google Cloud with gcloud (a browser opens)…");
    gcloud::login().map_err(|e| anyhow!(e))?;
    let auth = crate::gcp::auth::shared();
    auth.invalidate();
    auth.token()?;
    let how = auth.describe().unwrap_or_default();
    println!("{} {}", paint("✓ Signed in", "32"), paint(&format!("· gcli uses {how}"), "90"));
    Ok(ExitCode::SUCCESS)
}
