//! What a check found: the common language of every area's `--check` and of `gcli status`.
//!
//! An area with something to check exposes `findings(...) -> Result<Vec<Finding>>`. Its `--check`
//! prints them and exits with [`exit_code`], or [`COULD_NOT_TELL`] when it could not read.

use std::process::ExitCode;

use serde::Serialize;

/// Exit code of a check that could not read: distinct from 1, so a job alerting on 1 does not
/// mistake an outage of the check for a finding, and from 0, so it is not read as all clear.
pub const COULD_NOT_TELL: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// Worth a look soon; a check still exits 0.
    Warn,
    /// Needs attention; a check exits 1.
    Bad,
}

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub level: Level,
    /// What it is about: an app, a job, an instance.
    pub subject: String,
    /// What is wrong, in one short line.
    pub what: String,
    /// What to look at next, when there is something to say.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl Finding {
    pub fn bad(subject: impl Into<String>, what: impl Into<String>) -> Finding {
        Finding { level: Level::Bad, subject: subject.into(), what: what.into(), hint: None }
    }

    pub fn warn(subject: impl Into<String>, what: impl Into<String>) -> Finding {
        Finding { level: Level::Warn, subject: subject.into(), what: what.into(), hint: None }
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Finding {
        self.hint = Some(hint.into());
        self
    }
}

/// 1 when something needs attention, 0 otherwise.
pub fn exit_code(found: &[Finding]) -> ExitCode {
    if found.iter().any(|f| f.level == Level::Bad) { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

/// The worst first, then by subject.
pub fn sort(found: &mut [Finding]) {
    found.sort_by(|a, b| b.level.cmp(&a.level).then_with(|| a.subject.cmp(&b.subject)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_worst_comes_first_and_decides_the_exit() {
        let mut found = vec![Finding::warn("a", "x"), Finding::bad("c", "y"), Finding::bad("b", "z").hint("look")];
        sort(&mut found);
        assert_eq!(found.iter().map(|f| f.subject.as_str()).collect::<Vec<_>>(), ["b", "c", "a"]);
        assert_eq!(serde_json::to_value(&found[2]).unwrap()["level"], "warn");
        assert_eq!(serde_json::to_value(&found[0]).unwrap()["hint"], "look");
        assert!(serde_json::to_value(&found[1]).unwrap().get("hint").is_none());
        assert_eq!(exit_code(&found), ExitCode::FAILURE);
        assert_eq!(exit_code(&[Finding::warn("a", "x")]), ExitCode::SUCCESS);
    }
}
