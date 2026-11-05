//! What each app costs, as the Costs module last read it from the bill:
//! `~/.gcli/costs/<env>/apps.json`. The Apps list shows these columns only when the file is
//! there; Apps never queries the bill itself (BigQuery is billed per byte scanned).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::util::{fmt_ago, home_dir, parse_rfc3339};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CostFile {
    /// When the bill was read (RFC 3339).
    pub read_at: String,
    /// The last day whose costs are complete (`YYYY-MM-DD`).
    pub last_complete_day: String,
    /// By Cloud Run service id.
    pub apps: HashMap<String, AppCost>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppCost {
    /// USD, this month so far.
    pub month_to_date: f64,
    /// USD, the last complete day.
    pub yesterday: f64,
    /// USD, the median day of the 14 before it.
    pub median14: f64,
    /// Yesterday was a spike against that median.
    pub spike: bool,
}

impl CostFile {
    pub fn app(&self, id: &str) -> Option<&AppCost> {
        self.apps.get(id)
    }

    /// `read 3h ago, through 2026-11-04`
    pub fn freshness(&self) -> String {
        let read = parse_rfc3339(&self.read_at).map(fmt_ago).unwrap_or_else(|| self.read_at.clone());
        format!("costs read {read}, through {}", self.last_complete_day)
    }
}

/// The file for environment `env`. `None` for a name that is not a plain folder name.
pub fn path(env: &str) -> Option<PathBuf> {
    let plain = !env.is_empty() && env != "." && env != ".." && !env.contains(['/', '\\', ':']);
    plain.then(|| home_dir().map(|h| h.join(".gcli").join("costs").join(env).join("apps.json")))?
}

/// The costs of `env`'s apps: `None` when Costs has not written them; an error (to show, with
/// the columns hidden) when the file cannot be read.
pub fn read(env: &str) -> Result<Option<CostFile>, String> {
    path(env).map_or(Ok(None), |p| read_from(&p))
}

fn read_from(path: &Path) -> Result<Option<CostFile>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", crate::util::display_path(path))),
    };
    let mut file: CostFile = serde_json::from_str(&text)
        .map_err(|e| format!("{} is not what gcli costs writes: {e}", crate::util::display_path(path)))?;
    // Ids end up in the terminal.
    file.apps = file.apps.into_iter().map(|(k, v)| (crate::util::sanitize(&k), v)).collect();
    file.last_complete_day = crate::util::sanitize(&file.last_complete_day);
    Ok(Some(file))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str, text: Option<&str>) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gcli-costs-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("apps.json");
        let _ = std::fs::remove_file(&path);
        if let Some(t) = text {
            std::fs::write(&path, t).unwrap();
        }
        path
    }

    #[test]
    fn reads_the_costs_file_when_there_is_one() {
        let path = temp(
            "present",
            Some(
                r#"{"readAt":"2026-11-05T08:00:00Z","lastCompleteDay":"2026-11-04",
                    "apps":{"shop":{"monthToDate":41.5,"yesterday":9.2,"median14":3.1,"spike":true},"api":{"monthToDate":0.4}}}"#,
            ),
        );
        let file = read_from(&path).unwrap().unwrap();
        let shop = file.app("shop").unwrap();
        assert_eq!((shop.month_to_date, shop.yesterday, shop.median14, shop.spike), (41.5, 9.2, 3.1, true));
        assert_eq!((file.app("api").unwrap().yesterday, file.app("none").is_none()), (0.0, true));
        assert!(file.freshness().ends_with("through 2026-11-04"), "{}", file.freshness());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn no_file_hides_the_columns_and_a_broken_one_says_why() {
        assert!(read_from(&temp("absent", None)).unwrap().is_none());
        let broken = temp("broken", Some(r#"{"readAt":"x","apps":{"shop":{"monthToDate":"a lot"}}}"#));
        let e = read_from(&broken).unwrap_err();
        assert!(e.contains("is not what gcli costs writes"), "{e}");
        let truncated =
            temp("truncated", Some(r#"{"readAt":"2026-11-05T08:00:00Z","lastCompleteDay":"2026-11-04","apps":{"#));
        assert!(read_from(&truncated).is_err());
        std::fs::remove_dir_all(truncated.parent().unwrap()).unwrap();
        for name in ["absent", "broken"] {
            let _ =
                std::fs::remove_dir_all(std::env::temp_dir().join(format!("gcli-costs-{}-{name}", std::process::id())));
        }
        assert!(path("../etc").is_none() && path("").is_none() && path("a/b").is_none());
        assert!(path("prod").is_some_and(|p| p.ends_with(".gcli/costs/prod/apps.json")));
    }
}
