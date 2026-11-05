//! The YAML config, `~/.gcli/config.yaml`: one entry per environment (a project and a region,
//! and how its apps and its bill are found), plus defaults. Company specifics (project ids,
//! billing accounts, table names) live here, never in the code.
//!
//! Without a config file, the active gcloud configuration (its project and region, read from
//! gcloud's files) is the one environment.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::gcp::gcloud;
use crate::util::home_dir;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub environments: IndexMap<String, Environment>,
    #[serde(default)]
    pub defaults: Defaults,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Environment {
    /// The project id (`my-project-123`), not its number or display name.
    #[serde(default)]
    pub project: String,
    /// Empty: the gcloud configuration's region; still empty, every region.
    #[serde(default)]
    pub region: String,
    /// Changes in a production environment ask for its name to be typed. Unset: guessed
    /// from the environment's name and project ([`looks_like_production`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub production: Option<bool>,
    /// Where the bill is read (Costs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub billing: Option<Billing>,
    /// How apps are recognized among the project's resources.
    #[serde(default)]
    pub apps: AppsConvention,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Billing {
    /// The billing account (`012345-6789AB-CDEF01`).
    #[serde(default)]
    pub account: String,
    /// The detailed (resource-level) billing export: `project.dataset.table`.
    #[serde(default)]
    pub export_table: String,
    /// The project that runs, and pays for, the BigQuery jobs (`x-goog-user-project`).
    #[serde(default)]
    pub query_project: String,
}

/// What an app is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AppsConvention {
    /// An app is a Cloud Run service.
    #[default]
    Run,
    /// whiteghost's platform: the Cloud Run service `<slug>` is the app, its secrets are
    /// `<slug>-prod-*`, its load balancer pieces `<slug>-neg` and `<slug>-backend`.
    Whiteghost,
}

impl AppsConvention {
    pub fn as_str(self) -> &'static str {
        match self {
            AppsConvention::Run => "run",
            AppsConvention::Whiteghost => "whiteghost",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Defaults {
    /// Desktop notifications (a new cost spike, say), besides the toast.
    pub notifications: bool,
    pub costs: CostDefaults,
}

impl Default for Defaults {
    fn default() -> Self {
        Defaults { notifications: true, costs: CostDefaults::default() }
    }
}

/// When an app's day counts as a cost spike.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CostDefaults {
    /// Yesterday against the 14-day median of the same app.
    pub spike_ratio: f64,
    /// Spikes smaller than this (USD a day) are ignored.
    pub spike_min_usd: f64,
    /// An app with no history that costs more than this a day.
    pub new_app_min_usd: f64,
}

impl Default for CostDefaults {
    fn default() -> Self {
        CostDefaults { spike_ratio: 2.0, spike_min_usd: 1.0, new_app_min_usd: 2.0 }
    }
}

/// Where the config came from.
#[derive(Debug, Clone)]
pub enum Source {
    File(PathBuf),
    /// No config file: the active gcloud configuration, by this name.
    Gcloud(String),
}

#[derive(Debug, Clone)]
pub struct Loaded {
    pub config: Config,
    pub source: Source,
}

fn config_dir() -> Option<PathBuf> {
    Some(home_dir()?.join(".gcli"))
}

pub fn primary_path() -> Option<PathBuf> {
    Some(config_dir()?.join("config.yaml"))
}

/// Holds the environment chosen with `gcli use`, outside config.yaml so choosing one never
/// rewrites the config.
fn current_env_path() -> Option<PathBuf> {
    Some(config_dir()?.join("current-env"))
}

fn current_env_name() -> Option<String> {
    let text = std::fs::read_to_string(current_env_path()?).ok()?;
    Some(text.trim().to_string()).filter(|s| !s.is_empty())
}

pub fn set_current_env(name: &str) -> Result<()> {
    let path = current_env_path().ok_or_else(|| anyhow!("cannot find the home directory"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, format!("{name}\n")).with_context(|| format!("cannot write {}", path.display()))
}

fn search_paths() -> Vec<PathBuf> {
    let cwd = std::env::current_dir().unwrap_or_default();
    primary_path().into_iter().chain([cwd.join("gcli.yaml")]).collect()
}

pub fn load(explicit: Option<&Path>) -> Result<Loaded> {
    let path = match explicit {
        Some(p) => Some(p.to_path_buf()),
        None => search_paths().into_iter().find(|p| p.exists()),
    };
    let Some(path) = path else {
        let props = gcloud::active_properties();
        return Ok(Loaded { config: from_gcloud(&props), source: Source::Gcloud(props.name) });
    };
    let text = std::fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
    let config =
        parse(&text, &gcloud::active_properties()).with_context(|| format!("invalid config {}", path.display()))?;
    Ok(Loaded { config, source: Source::File(path) })
}

/// Reads a config file's text; `gcloud` fills the regions it leaves out.
fn parse(text: &str, gcloud: &gcloud::Properties) -> Result<Config> {
    let mut config: Config = serde_saphyr::from_str(text).map_err(|e| anyhow!("{e}"))?;
    config.sanitize_text();
    for env in config.environments.values_mut() {
        if env.region.is_empty() {
            env.region = gcloud.region.clone().unwrap_or_default();
        }
    }
    config.validate()?;
    Ok(config)
}

/// The one environment of the active gcloud configuration, named after its project; none
/// when it has no project.
fn from_gcloud(props: &gcloud::Properties) -> Config {
    let mut config = Config::default();
    if let Some(project) = &props.project {
        let env = Environment {
            project: project.clone(),
            region: props.region.clone().unwrap_or_default(),
            ..Default::default()
        };
        config.environments.insert(project.clone(), env);
    }
    config
}

/// Environments whose name or project suggests production, when the config does not say.
pub fn looks_like_production(text: &str) -> bool {
    let n = text.to_ascii_lowercase();
    n.contains("prod") || n.contains("prd") || n.contains("live")
}

impl Environment {
    /// Whether changes here ask for the environment's name to be typed (`name`: its name).
    pub fn is_production(&self, name: &str) -> bool {
        self.production.unwrap_or_else(|| looks_like_production(name) || looks_like_production(&self.project))
    }

    /// `project · region` for lists and the header.
    pub fn summary(&self) -> String {
        let region = if self.region.is_empty() { "all regions" } else { self.region.as_str() };
        format!("{} · {region}", self.project)
    }
}

impl Config {
    /// Names end up in the terminal; strip control sequences from them.
    fn sanitize_text(&mut self) {
        use crate::util::sanitize;
        self.environments = std::mem::take(&mut self.environments)
            .into_iter()
            .map(|(name, mut env)| {
                env.project = sanitize(env.project.trim());
                env.region = sanitize(env.region.trim());
                if let Some(b) = &mut env.billing {
                    b.account = sanitize(b.account.trim());
                    b.export_table = sanitize(b.export_table.trim());
                    b.query_project = sanitize(b.query_project.trim());
                }
                (sanitize(&name), env)
            })
            .collect();
    }

    fn validate(&self) -> Result<()> {
        if self.environments.is_empty() {
            bail!("no environments defined");
        }
        for (name, env) in &self.environments {
            if env.project.is_empty() {
                bail!("environment '{name}' has no project");
            }
            if let Some(table) = env.billing.as_ref().map(|b| &b.export_table).filter(|t| !t.is_empty())
                && table.split('.').filter(|p| !p.is_empty()).count() != 3
            {
                bail!("environment '{name}': billing.exportTable is project.dataset.table, not '{table}'");
            }
        }
        let c = &self.defaults.costs;
        // Written so that NaN fails too.
        let sensible = c.spike_ratio >= 1.0 && c.spike_min_usd >= 0.0 && c.new_app_min_usd >= 0.0;
        if !sensible {
            bail!("defaults.costs: spikeRatio is at least 1, spikeMinUsd and newAppMinUsd at least 0");
        }
        Ok(())
    }

    /// The environment used when none is given: the one chosen with `use`, else the first.
    pub fn default_env(&self) -> Option<&str> {
        current_env_name()
            .and_then(|n| self.environments.get_key_value(&n).map(|(k, _)| k.as_str()))
            .or_else(|| self.environments.keys().next().map(String::as_str))
    }

    pub fn env(&self, name: &str) -> Result<&Environment> {
        self.environments.get(name).ok_or_else(|| {
            let names: Vec<&str> = self.environments.keys().map(String::as_str).collect();
            anyhow!("unknown environment '{name}'. Available: {}", names.join(", "))
        })
    }
}

// ---------------------------------------------------------------------------
// Editing. Changes go through a generic YAML document, so keys this version does not
// know survive (a newer gcli's, or your own notes as keys); comments and formatting do not.
// ---------------------------------------------------------------------------

/// What `gcli add` sets on an environment; `None` leaves a field as it is.
#[derive(Debug, Default)]
pub struct EnvChange {
    pub project: Option<String>,
    pub region: Option<String>,
    pub production: Option<bool>,
}

impl Loaded {
    /// The file edits are written to: the loaded one, or the primary path when there was none.
    pub fn edit_path(&self) -> Result<PathBuf> {
        match &self.source {
            Source::File(p) => Ok(p.clone()),
            Source::Gcloud(_) => primary_path().ok_or_else(|| anyhow!("cannot find the home directory")),
        }
    }

    /// Adds environment `name`, or changes the fields `change` sets on an existing one, then
    /// saves and reloads. Returns the file written and whether the environment is new.
    pub fn set_env(&mut self, name: &str, change: &EnvChange) -> Result<(PathBuf, bool)> {
        let mut created = false;
        let path = self.edit(|root| {
            let envs = root.entry("environments").or_insert_with(|| json!({}));
            if envs.is_null() {
                *envs = json!({});
            }
            let envs = envs.as_object_mut().ok_or_else(|| anyhow!("'environments' is not a mapping"))?;
            created = !envs.contains_key(name);
            let entry = envs
                .entry(name)
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .ok_or_else(|| anyhow!("environment '{name}' is not a mapping"))?;
            if let Some(p) = &change.project {
                entry.insert("project".into(), json!(p));
            }
            if let Some(r) = &change.region {
                entry.insert("region".into(), json!(r));
            }
            if let Some(p) = change.production {
                entry.insert("production".into(), json!(p));
            }
            Ok(())
        })?;
        Ok((path, created))
    }

    fn edit(&mut self, change: impl FnOnce(&mut Map<String, Value>) -> Result<()>) -> Result<PathBuf> {
        let path = self.edit_path()?;
        let original = std::fs::read_to_string(&path).ok();
        let mut doc: Value = match &original {
            Some(text) => {
                serde_saphyr::from_str(text).map_err(|e| anyhow!("invalid config {}:\n{e}", path.display()))?
            }
            None => json!({ "environments": {} }),
        };
        if doc.is_null() {
            doc = json!({});
        }
        let root = doc.as_object_mut().ok_or_else(|| anyhow!("{} is not a YAML mapping", path.display()))?;
        change(root)?;
        let yaml = serde_saphyr::to_string(&doc).map_err(|e| anyhow!("cannot write YAML: {e}"))?;
        // Checked before anything is written: a change that breaks the config is refused.
        parse(&yaml, &gcloud::Properties::default()).with_context(|| format!("not saved to {}", path.display()))?;

        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let write =
            |p: &Path, text: &str| std::fs::write(p, text).with_context(|| format!("cannot write {}", p.display()));
        if let Some(text) = &original {
            write(&path.with_extension("yaml.bak"), text)?;
        }
        // Replace atomically, unless the config is a symlink (dotfiles) that must stay one.
        let is_link = std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink());
        if is_link {
            write(&path, &yaml)?;
        } else {
            let tmp = path.with_extension("yaml.tmp");
            write(&tmp, &yaml)?;
            std::fs::rename(&tmp, &path).with_context(|| format!("cannot replace {}", path.display()))?;
        }
        *self = load(Some(&path))?;
        Ok(path)
    }
}
