//! `gcli update` (replace this binary with the latest GitHub release, or update through npm
//! when installed that way) and a daily, cached check that tells the UI a newer version exists.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};

use crate::gcp::http;
use crate::util::{hex, home_dir, now_secs};

const REPO: &str = "castri1/gcli";
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
/// Seconds between two checks for a newer version.
const CHECK_EVERY: f64 = 24.0 * 3600.0;

fn get(url: &str, accept: &str) -> Result<Vec<u8>> {
    let headers = [("accept".to_string(), accept.to_string())];
    let resp = http::send("GET", url, &headers, Vec::new()).map_err(|e| anyhow!("{e}"))?;
    if !resp.ok() {
        bail!("GET {url}: HTTP {}", resp.status);
    }
    Ok(resp.body)
}

/// Tag of the latest release (`v0.2.0`).
pub fn latest_tag() -> Result<String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let headers = [("accept".to_string(), "application/vnd.github+json".to_string())];
    let resp = http::send("GET", &url, &headers, Vec::new()).map_err(|e| anyhow!("{e}"))?;
    // Before the first release (or while the repository is private) GitHub answers 404.
    if resp.status == 404 {
        bail!("no gcli release found in github.com/{REPO} yet");
    }
    if !resp.ok() {
        bail!("GET {url}: HTTP {}", resp.status);
    }
    let body = resp.body;
    let json: serde_json::Value = serde_json::from_slice(&body)?;
    json["tag_name"].as_str().map(str::to_string).ok_or_else(|| anyhow!("no release found in github.com/{REPO}"))
}

fn version(s: &str) -> Option<(u64, u64, u64)> {
    let mut parts = s.trim().trim_start_matches('v').split('.').map(|p| p.parse::<u64>().ok());
    Some((parts.next()??, parts.next()??, parts.next().flatten().unwrap_or(0)))
}

/// True when `tag` is a newer version than `current`.
pub fn is_newer(tag: &str, current: &str) -> bool {
    matches!((version(tag), version(current)), (Some(a), Some(b)) if a > b)
}

fn cache_path() -> Option<PathBuf> {
    Some(home_dir()?.join(".gcli").join("update-check"))
}

/// The latest tag, from a cache refreshed at most once a day. `None` when the check is
/// disabled (`GCLI_NO_UPDATE_CHECK`), offline or up to date.
pub fn newer_version_available() -> Option<String> {
    if std::env::var_os("GCLI_NO_UPDATE_CHECK").is_some() {
        return None;
    }
    let path = cache_path()?;
    let cached = std::fs::read_to_string(&path).ok().and_then(|text| {
        let (at, tag) = text.trim().split_once(' ')?;
        Some((at.parse::<f64>().ok()?, tag.to_string()))
    });
    let tag = match cached {
        Some((at, tag)) if now_secs() - at < CHECK_EVERY => tag,
        _ => {
            let tag = latest_tag().ok()?;
            remember(&path, &tag);
            tag
        }
    };
    is_newer(&tag, CURRENT).then_some(tag)
}

fn remember(path: &Path, tag: &str) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, format!("{} {tag}\n", now_secs() as u64));
}

/// The release archive name for this build's platform.
fn target() -> Result<&'static str> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        // Windows on ARM runs the x64 build.
        ("windows", _) => "x86_64-pc-windows-msvc",
        (os, arch) => bail!("no prebuilt gcli for {os} {arch}; build it with cargo"),
    })
}

/// `gcli update`: `check` only reports; `to` installs that release tag (even an older one).
pub fn run(check: bool, to: Option<String>) -> Result<ExitCode> {
    let pinned = to.is_some();
    let tag = match to {
        Some(t) => format!("v{}", t.trim_start_matches('v')),
        None => {
            let tag = latest_tag()?;
            if let Some(path) = cache_path() {
                remember(&path, &tag);
            }
            tag
        }
    };
    if !pinned && !is_newer(&tag, CURRENT) {
        println!("gcli {CURRENT} is up to date.");
        return Ok(ExitCode::SUCCESS);
    }
    if !pinned {
        println!("gcli {} is available (you have {CURRENT}).", tag.trim_start_matches('v'));
    }
    if check {
        println!("Update with: gcli update");
        return Ok(ExitCode::SUCCESS);
    }

    let exe = std::env::current_exe()?.canonicalize()?;
    if exe.components().any(|c| c.as_os_str() == "node_modules") {
        let spec = format!("@castri1/gcli@{}", tag.trim_start_matches('v'));
        if cfg!(windows) {
            // npm cannot replace gcli.exe while it runs.
            println!("Installed with npm. Close gcli and run: npm install -g {spec}");
            return Ok(ExitCode::SUCCESS);
        }
        println!("Installed with npm: running npm install -g {spec}");
        let status = std::process::Command::new("npm").args(["install", "-g", &spec]).status()?;
        return Ok(if status.success() { ExitCode::SUCCESS } else { ExitCode::FAILURE });
    }

    let target = target()?;
    let name = format!("gcli-{tag}-{target}");
    let archive = if cfg!(windows) { format!("{name}.zip") } else { format!("{name}.tar.gz") };
    let url = format!("https://github.com/{REPO}/releases/download/{tag}/{archive}");
    println!("Downloading {archive}…");
    let bytes = get(&url, "application/octet-stream")?;
    let sums = String::from_utf8(get(&format!("{url}.sha256"), "application/octet-stream")?)?;
    let expected = sums.split_whitespace().next().unwrap_or_default().to_lowercase();
    if hex(&Sha256::digest(&bytes)) != expected {
        bail!("checksum mismatch for {archive}; not installed");
    }

    let dir = private_temp_dir()?;
    let result = (|| -> Result<()> {
        let archive_path = dir.join(&archive);
        std::fs::write(&archive_path, &bytes)?;
        // bsdtar ships with macOS and Windows 10+ and reads zip too; GNU tar on Linux.
        let flags = if cfg!(windows) { "-xf" } else { "-xzf" };
        let status = std::process::Command::new("tar").arg(flags).arg(&archive_path).arg("-C").arg(&dir).status()?;
        if !status.success() {
            bail!("could not extract {archive} (tar exited with {status})");
        }
        let binary = dir.join(&name).join(if cfg!(windows) { "gcli.exe" } else { "gcli" });
        replace(&exe, &binary)
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result?;
    println!("✓ Updated {} to gcli {}", exe.display(), tag.trim_start_matches('v'));
    Ok(ExitCode::SUCCESS)
}

/// A new directory only this user can read, in the temp folder.
fn private_temp_dir() -> Result<PathBuf> {
    for attempt in 0..8u32 {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        let dir =
            std::env::temp_dir().join(format!("gcli-update-{:x}", std::process::id() ^ nanos.rotate_left(attempt)));
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt;
            let mut b = std::fs::DirBuilder::new();
            b.mode(0o700);
            b
        };
        #[cfg(not(unix))]
        let builder = std::fs::DirBuilder::new();
        match builder.create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    bail!("could not create a temporary folder")
}

/// Puts `new` in place of the running binary `exe`. Unix replaces it atomically; Windows
/// cannot overwrite a running .exe but can rename it, so the old one is moved aside and
/// deleted on a later start ([`cleanup`]).
fn replace(exe: &Path, new: &Path) -> Result<()> {
    let staged = exe.with_file_name(".gcli-update");
    std::fs::copy(new, &staged).with_context(|| denied_hint(exe))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(windows)]
    {
        let old = exe.with_extension("exe.old");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(exe, &old).with_context(|| denied_hint(exe))?;
    }
    std::fs::rename(&staged, exe).with_context(|| denied_hint(exe))
}

fn denied_hint(exe: &Path) -> String {
    format!(
        "cannot replace {}. If it needs admin rights, reinstall with the install script: https://github.com/{REPO}#install",
        exe.display()
    )
}

/// Removes the binary a Windows update left aside.
pub fn cleanup() {
    if cfg!(windows)
        && let Ok(exe) = std::env::current_exe()
    {
        let _ = std::fs::remove_file(exe.with_extension("exe.old"));
    }
}
