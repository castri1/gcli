//! `~/.gcli/costs/ledger.json`: what gcli's BigQuery reads were billed, per month. BigQuery
//! bills the bytes each query scans: the Costs module and `gcli costs` add every query here,
//! and show the month's total next to each new estimate.
//!
//! Changes go through [`count`]: one at a time in the process (reads run on background
//! threads), and never over a file that cannot be read back, which would silently lose the
//! month's count.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Month {
    pub queries: u64,
    /// Bytes billed (dry runs are free and not counted).
    pub bytes: u64,
}

pub type Ledger = BTreeMap<String, Month>;

pub fn path() -> Option<PathBuf> {
    Some(super::costs_dir()?.join("ledger.json"))
}

fn read(path: &Path) -> std::io::Result<Ledger> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|e| std::io::Error::other(format!("{} is not valid ({e}): fix or delete it", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Ledger::new()),
        Err(e) => Err(e),
    }
}

static LOCK: Mutex<()> = Mutex::new(());

/// Adds `queries` queries billed `bytes` to `month` (`2026-11`) in the ledger at `path`.
pub fn count_at(path: &Path, month: &str, queries: u64, bytes: u64) -> std::io::Result<()> {
    let _one = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut ledger = read(path)?;
    let m = ledger.entry(month.to_string()).or_default();
    m.queries += queries;
    m.bytes = m.bytes.saturating_add(bytes);
    super::write_private(path, (serde_json::to_string_pretty(&ledger).unwrap_or_default() + "\n").as_bytes())
}

/// Adds a read to this month's count. Best effort: a ledger that cannot be written does not
/// lose the read itself.
pub fn count(month: &str, queries: u64, bytes: u64) {
    if let Some(p) = path() {
        let _ = count_at(&p, month, queries, bytes);
    }
}

/// What `month` has been billed so far.
pub fn month(month: &str) -> Month {
    path().and_then(|p| read(&p).ok()).and_then(|l| l.get(month).cloned()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_add_up_per_month_and_bad_files_are_kept() {
        let dir = std::env::temp_dir().join(format!("gcli-ledger-{}", std::process::id()));
        let path = dir.join("ledger.json");
        count_at(&path, "2026-10", 3, 1_000).unwrap();
        count_at(&path, "2026-11", 1, 500).unwrap();
        count_at(&path, "2026-11", 2, 250).unwrap();
        let ledger = read(&path).unwrap();
        assert_eq!(ledger["2026-10"], Month { queries: 3, bytes: 1_000 });
        assert_eq!(ledger["2026-11"], Month { queries: 3, bytes: 750 });
        std::fs::write(&path, "{broken").unwrap();
        assert!(count_at(&path, "2026-11", 1, 1).is_err(), "never written over a file it cannot read");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{broken");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
