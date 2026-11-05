//! What gcli takes from the gcloud CLI: its configuration (the active project and region, read
//! from its files, never by running it), an access token when Application Default Credentials
//! cannot give one, and the sign-in (`gcloud auth login --update-adc`).

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const INSTALL_URL: &str = "https://cloud.google.com/sdk/docs/install";

/// How long `gcloud auth print-access-token` may take (it refreshes over the network).
const TOKEN_TIMEOUT: Duration = Duration::from_secs(60);

/// gcloud's configuration folder: `CLOUDSDK_CONFIG`, else `~/.config/gcloud`
/// (`%APPDATA%\gcloud` on Windows).
pub fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLOUDSDK_CONFIG").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    if cfg!(windows) {
        return std::env::var_os("APPDATA").filter(|d| !d.is_empty()).map(|d| PathBuf::from(d).join("gcloud"));
    }
    Some(crate::util::home_dir()?.join(".config").join("gcloud"))
}

/// `gcloud` as a command. On Windows it is a batch file, which `Command` finds only by its
/// full name.
pub fn command() -> Command {
    Command::new(if cfg!(windows) { "gcloud.cmd" } else { "gcloud" })
}

/// Signs in to gcloud and writes Application Default Credentials in the same browser flow.
pub fn login_command() -> Command {
    let mut c = command();
    c.args(["auth", "login", "--update-adc"]);
    c
}

/// Runs the sign-in in this terminal (it opens a browser, and may ask something), returning
/// once it ends. The caller hands it the terminal: the CLI as is, the UI suspended.
pub fn login() -> Result<(), String> {
    let status = login_command().status().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            format!("gcloud is not installed: {INSTALL_URL}")
        } else {
            format!("cannot run gcloud: {e}")
        }
    })?;
    if status.success() { Ok(()) } else { Err(format!("gcloud auth login exited with {status}")) }
}

/// The active configuration's properties that gcli uses.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Properties {
    /// The configuration's name (`default`).
    pub name: String,
    pub project: Option<String>,
    /// `run/region`, else `compute/region`.
    pub region: Option<String>,
}

/// The active configuration, read from gcloud's files, with the `CLOUDSDK_*` variables that
/// gcloud itself would honour on top.
pub fn active_properties() -> Properties {
    let var = |name: &str| std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let mut props = config_dir().map(|dir| properties_in(&dir, var("CLOUDSDK_ACTIVE_CONFIG_NAME"))).unwrap_or_default();
    if let Some(project) = var("CLOUDSDK_CORE_PROJECT") {
        props.project = Some(project);
    }
    if let Some(region) = var("CLOUDSDK_RUN_REGION").or_else(|| var("CLOUDSDK_COMPUTE_REGION")) {
        props.region = Some(region);
    }
    props
}

/// The properties of the configuration named `active` (default: the one in `active_config`)
/// in gcloud's folder `dir`.
pub fn properties_in(dir: &Path, active: Option<String>) -> Properties {
    let name = active
        .or_else(|| std::fs::read_to_string(dir.join("active_config")).ok().map(|s| s.trim().to_string()))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "default".into());
    // A configuration name is a file name: never a path.
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Properties { name, ..Default::default() };
    }
    let text = std::fs::read_to_string(dir.join("configurations").join(format!("config_{name}"))).unwrap_or_default();
    let ini = parse_ini(&text);
    let get = |section: &str, key: &str| {
        ini.get(&(section.to_string(), key.to_string())).map(|v| crate::util::sanitize(v)).filter(|v| !v.is_empty())
    };
    Properties {
        project: get("core", "project"),
        region: get("run", "region").or_else(|| get("compute", "region")),
        name,
    }
}

/// `[section]` / `key = value` lines, as gcloud writes its configurations.
fn parse_ini(text: &str) -> HashMap<(String, String), String> {
    let mut section = String::new();
    let mut out = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name.trim().to_string();
        } else if let Some((key, value)) = line.split_once('=') {
            out.insert((section.clone(), key.trim().to_string()), value.trim().to_string());
        }
    }
    out
}

/// `gcloud auth print-access-token`: the token, or gcloud's own explanation (its first error
/// line) of why there is none. Never prompts: stdin is closed, so a reauthentication fails
/// instead of waiting for input nobody sees.
pub fn print_access_token() -> Result<String, String> {
    let mut child = match command()
        .args(["auth", "print-access-token", "--quiet"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!("gcloud is not installed ({INSTALL_URL})"));
        }
        Err(e) => return Err(format!("cannot run gcloud: {e}")),
    };
    // Read both pipes on their own threads so a chatty gcloud never blocks on a full pipe.
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let out = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(p) = stdout.as_mut() {
            let _ = p.read_to_string(&mut s);
        }
        s
    });
    let err = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(p) = stderr.as_mut() {
            let _ = p.read_to_string(&mut s);
        }
        s
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() > TOKEN_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("gcloud auth print-access-token took more than {}s", TOKEN_TIMEOUT.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(e) => return Err(format!("gcloud: {e}")),
        }
    };
    let token = out.join().unwrap_or_default().trim().to_string();
    let stderr = err.join().unwrap_or_default();
    if status.success() && !token.is_empty() && !token.contains(char::is_whitespace) {
        return Ok(token);
    }
    Err(explain_failure(&stderr).unwrap_or_else(|| format!("gcloud auth print-access-token exited with {status}")))
}

/// The useful part of gcloud's stderr: its `ERROR:` line without the command prefix
/// (`ERROR: (gcloud.auth.print-access-token) Reauthentication failed. …`).
fn explain_failure(stderr: &str) -> Option<String> {
    let line = stderr
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("ERROR:"))
        .or_else(|| stderr.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with("WARNING:")))?;
    let line = line.trim_start_matches("ERROR:").trim();
    let line = match line.strip_prefix('(').and_then(|l| l.split_once(')')) {
        Some((_, rest)) => rest.trim(),
        None => line,
    };
    Some(super::error::redact(line)).filter(|l| !l.is_empty())
}
