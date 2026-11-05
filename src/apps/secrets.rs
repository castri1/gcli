//! An app's secrets: which exist (by the environment's convention), which the service reads
//! and which version each read gets, and whether the running revision has the newest one.
//!
//! A variable bound to `latest` is resolved once, when an instance starts: a version added
//! later is not seen until instances start again (a restart makes them). A version that is
//! disabled or destroyed makes new instances fail to start. Values are never read.

use std::collections::BTreeMap;

use crate::config::AppsConvention;
use crate::gcp::run::{self, SecretRef};
use crate::gcp::secrets::{self as sm, Version};
use crate::gcp::{Gcp, GcpError, Result};
use crate::util::{fmt_ago, fmt_utc};

/// A secret of the app, with its versions.
#[derive(Debug, Clone)]
pub struct Secret {
    /// Its id, or `projects/{p}/secrets/{id}` for another project's.
    pub key: String,
    /// The resource name, to read its versions.
    pub resource: String,
    /// Empty when it was found only through a reference.
    pub create_time: String,
    /// Named after the app by the convention (rather than only read by it).
    pub own: bool,
    pub versions: std::result::Result<Vec<Version>, GcpError>,
}

/// How the service refers to a secret, as a key: its id in the service's own project,
/// `projects/{p}/secrets/{id}` in another.
pub fn key(project: &str, secret: &str) -> String {
    match secret.strip_prefix(&format!("projects/{project}/secrets/")) {
        Some(id) => id.to_string(),
        None => secret.to_string(),
    }
}

/// The app's secrets: the convention's (`<slug>-prod-*`) and every other secret the service
/// reads, each with its versions (read at the same time).
pub fn load(gcp: &Gcp, convention: AppsConvention, service: &run::Service) -> Result<Vec<Secret>> {
    let mut found: BTreeMap<String, Secret> = BTreeMap::new();
    if let Some(prefix) = convention.secret_prefix(service.id()) {
        for s in sm::with_prefix(gcp, &prefix)? {
            let key = s.id().to_string();
            found.insert(
                key.clone(),
                Secret {
                    key,
                    resource: s.name.clone(),
                    create_time: s.create_time,
                    own: true,
                    versions: Ok(Vec::new()),
                },
            );
        }
    }
    for r in service.secret_refs() {
        let key = key(&gcp.project, &r.secret);
        found.entry(key.clone()).or_insert_with(|| Secret {
            resource: sm::resource(&gcp.project, &r.secret),
            key,
            create_time: String::new(),
            own: false,
            versions: Ok(Vec::new()),
        });
    }
    let mut secrets: Vec<Secret> = found.into_values().collect();
    let resources: Vec<String> = secrets.iter().map(|s| s.resource.clone()).collect();
    let versions: Vec<_> = std::thread::scope(|s| {
        let reads: Vec<_> = resources.iter().map(|r| s.spawn(move || sm::versions(gcp, r))).collect();
        reads
            .into_iter()
            .map(|read| read.join().unwrap_or_else(|_| Err(GcpError::Network("reading the versions failed".into()))))
            .collect()
    });
    for (secret, read) in secrets.iter_mut().zip(versions) {
        secret.versions = read;
    }
    Ok(secrets)
}

/// Something to know about a secret binding.
#[derive(Debug, Clone, PartialEq)]
pub enum Finding {
    /// Bound as `latest` to a variable, and its newest version came after the running revision:
    /// instances started before it still have the old value.
    NotLoaded { target: String, version: u64, created: f64 },
    /// The version read is disabled, destroyed or missing: new instances fail to start.
    Unusable { target: String, version: String, state: String },
    /// Pinned to a version while a newer enabled one exists.
    Pinned { target: String, pinned: String, newest: u64 },
}

impl Finding {
    /// Breaks something now (or at the next start), rather than worth knowing.
    pub fn serious(&self) -> bool {
        !matches!(self, Finding::Pinned { .. })
    }

    pub fn text(&self) -> String {
        match self {
            Finding::NotLoaded { target, version, created } => format!(
                "{target}: new version {version} ({}, {}) not loaded yet: restart to pick it up",
                fmt_utc(*created),
                fmt_ago(*created)
            ),
            Finding::Unusable { target, version, state } => {
                format!("{target}: reads version {version}, which is {state}: new instances fail to start")
            }
            Finding::Pinned { target, pinned, newest } => {
                format!("{target}: pinned to version {pinned}; version {newest} is newer")
            }
        }
    }
}

/// A secret with what reads it and what to know.
#[derive(Debug, Clone)]
pub struct State<'a> {
    pub secret: &'a Secret,
    pub bindings: Vec<SecretRef>,
    pub findings: Vec<Finding>,
}

impl State<'_> {
    pub fn bound(&self) -> bool {
        !self.bindings.is_empty()
    }
}

/// Each secret with the bindings of `refs` (the running template's) and their findings: those
/// with something serious first, then the bound ones, then the rest. `revision_created` is
/// when the running revision was made (its instances started after).
pub fn states<'a>(
    project: &str,
    secrets: &'a [Secret],
    refs: &[SecretRef],
    revision_created: Option<f64>,
) -> Vec<State<'a>> {
    let mut out = secrets
        .iter()
        .map(|secret| {
            let bindings: Vec<SecretRef> =
                refs.iter().filter(|r| key(project, &r.secret) == secret.key).cloned().collect();
            let findings = match &secret.versions {
                Ok(versions) => bindings.iter().filter_map(|b| finding(b, versions, revision_created)).collect(),
                Err(_) => Vec::new(),
            };
            State { secret, bindings, findings }
        })
        .collect::<Vec<_>>();
    out.sort_by_key(|st| (!st.findings.iter().any(Finding::serious), !st.bound(), st.secret.key.clone()));
    out
}

/// What to know about one binding, given the secret's versions (newest first).
fn finding(binding: &SecretRef, versions: &[Version], revision_created: Option<f64>) -> Option<Finding> {
    let target = binding.target.clone();
    let newest_enabled = versions.iter().find(|v| v.enabled());
    if binding.version == "latest" {
        // `latest` is the most recently created version, whatever its state.
        let Some(newest) = versions.first() else {
            return Some(Finding::Unusable { target, version: "latest".into(), state: "missing (no versions)".into() });
        };
        if !newest.enabled() {
            let state = newest.state.to_lowercase();
            return Some(Finding::Unusable { target, version: format!("latest = {}", newest.number()), state });
        }
        // A mounted file is read on every access: it always has the newest.
        let created = newest.created()?;
        return (!binding.mounted && revision_created.is_some_and(|r| created > r)).then(|| Finding::NotLoaded {
            target,
            version: newest.number(),
            created,
        });
    }
    match versions.iter().find(|v| v.number().to_string() == binding.version) {
        None => Some(Finding::Unusable { target, version: binding.version.clone(), state: "missing".into() }),
        Some(v) if !v.enabled() => {
            Some(Finding::Unusable { target, version: binding.version.clone(), state: v.state.to_lowercase() })
        }
        Some(v) => newest_enabled.filter(|n| n.number() > v.number()).map(|n| Finding::Pinned {
            target,
            pinned: binding.version.clone(),
            newest: n.number(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gcp::mock::{MockServer, Reply};
    use crate::util::parse_rfc3339;

    fn version(n: u64, created: &str, state: &str) -> Version {
        Version {
            name: format!("projects/1/secrets/s/versions/{n}"),
            create_time: created.into(),
            state: state.into(),
            destroy_time: String::new(),
        }
    }

    fn bind(target: &str, version: &str, mounted: bool) -> SecretRef {
        SecretRef { target: target.into(), secret: "s".into(), version: version.into(), mounted }
    }

    #[test]
    fn a_version_newer_than_the_revision_is_not_loaded_yet() {
        let revision = parse_rfc3339("2026-11-01T10:00:00Z");
        let versions = [version(3, "2026-11-02T09:00:00Z", "ENABLED"), version(2, "2026-10-01T00:00:00Z", "ENABLED")];
        let f = finding(&bind("MCP_API_TOKEN", "latest", false), &versions, revision).unwrap();
        assert!(matches!(&f, Finding::NotLoaded { version: 3, .. }), "{f:?}");
        assert!(f.serious());
        assert!(f.text().starts_with("MCP_API_TOKEN: new version 3 (2026-11-02 09:00 UTC"), "{}", f.text());
        assert!(f.text().ends_with("not loaded yet: restart to pick it up"));
        // Made before the revision: loaded.
        assert_eq!(finding(&bind("T", "latest", false), &versions[1..], revision), None);
        // A mounted file reads the newest on every access.
        assert_eq!(finding(&bind("/s/t", "latest", true), &versions, revision), None);
        // When the revision's time is unknown, nothing is claimed.
        assert_eq!(finding(&bind("T", "latest", false), &versions, None), None);
    }

    #[test]
    fn unusable_and_pinned_versions() {
        let versions = [
            version(4, "2026-11-03T00:00:00Z", "DISABLED"),
            version(3, "2026-11-02T00:00:00Z", "ENABLED"),
            version(2, "2026-10-02T00:00:00Z", "DESTROYED"),
            version(1, "2026-10-01T00:00:00Z", "ENABLED"),
        ];
        let revision = parse_rfc3339("2026-11-05T00:00:00Z");
        let latest = finding(&bind("A", "latest", false), &versions, revision).unwrap();
        assert_eq!(
            latest,
            Finding::Unusable { target: "A".into(), version: "latest = 4".into(), state: "disabled".into() }
        );
        let destroyed = finding(&bind("B", "2", false), &versions, revision).unwrap();
        assert!(destroyed.text().contains("version 2, which is destroyed: new instances fail to start"));
        let missing = finding(&bind("C", "9", false), &versions, revision).unwrap();
        assert!(matches!(missing, Finding::Unusable { ref state, .. } if state == "missing"));
        let pinned = finding(&bind("D", "1", false), &versions, revision).unwrap();
        assert_eq!(pinned, Finding::Pinned { target: "D".into(), pinned: "1".into(), newest: 3 });
        assert!(!pinned.serious());
        assert_eq!(finding(&bind("E", "3", false), &versions, revision), None);
        assert!(matches!(finding(&bind("F", "latest", false), &[], revision), Some(Finding::Unusable { .. })));
    }

    #[test]
    fn bound_and_unbound_secrets_of_an_app() {
        let server = MockServer::start(vec![]);
        server.route(
            "/secrets?filter",
            Reply::json(
                200,
                r#"{"secrets":[
                    {"name":"projects/123/secrets/shop-prod-token","createTime":"2026-10-01T00:00:00Z"},
                    {"name":"projects/123/secrets/shop-prod-token-v2","createTime":"2026-10-05T00:00:00Z"},
                    {"name":"projects/123/secrets/shop-prod-database-url","createTime":"2026-09-01T00:00:00Z"}]}"#,
            ),
        );
        server.route(
            "shop-prod-token/versions",
            Reply::json(
                200,
                r#"{"versions":[{"name":"projects/123/secrets/shop-prod-token/versions/2","createTime":"2026-11-02T09:00:00Z","state":"ENABLED"},
                               {"name":"projects/123/secrets/shop-prod-token/versions/1","createTime":"2026-10-01T00:00:00Z","state":"DESTROYED"}]}"#,
            ),
        );
        server.route(
            "shop-prod-token-v2/versions",
            Reply::json(200, r#"{"versions":[{"name":"projects/123/secrets/shop-prod-token-v2/versions/1","createTime":"2026-10-05T00:00:00Z","state":"ENABLED"}]}"#),
        );
        server.route(
            "shop-prod-database-url/versions",
            Reply::json(200, r#"{"versions":[{"name":"projects/123/secrets/shop-prod-database-url/versions/1","createTime":"2026-09-01T00:00:00Z","state":"ENABLED"}]}"#),
        );
        server.route(
            "shared-key/versions",
            Reply::json(
                403,
                r#"{"error":{"code":403,"status":"PERMISSION_DENIED","message":"no access to the other project"}}"#,
            ),
        );
        let service: run::Service = serde_json::from_str(
            r#"{"name":"projects/p/locations/us-east1/services/shop","template":{"containers":[{"env":[
                {"name":"MCP_API_TOKEN","valueSource":{"secretKeyRef":{"secret":"shop-prod-token","version":"latest"}}},
                {"name":"DATABASE_URL","valueSource":{"secretKeyRef":{"secret":"projects/p/secrets/shop-prod-database-url","version":"latest"}}},
                {"name":"API_KEY","valueSource":{"secretKeyRef":{"secret":"projects/other/secrets/shared-key","version":"3"}}}]}]}}"#,
        )
        .unwrap();
        let gcp = Gcp::with("p", "us-east1", crate::gcp::auth::tests::fixed("t"), Some(server.url()));
        let secrets = load(&gcp, AppsConvention::Whiteghost, &service).unwrap();
        let keys: Vec<&str> = secrets.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(
            keys,
            ["projects/other/secrets/shared-key", "shop-prod-database-url", "shop-prod-token", "shop-prod-token-v2"]
        );
        assert!(server.requests().iter().all(|r| !r.path.contains(":access") && !r.path.contains("%3Aaccess")));

        let revision = parse_rfc3339("2026-11-01T10:00:00Z");
        let states = states("p", &secrets, &service.secret_refs(), revision);
        let summary: Vec<(&str, bool, usize)> =
            states.iter().map(|s| (s.secret.key.as_str(), s.bound(), s.findings.len())).collect();
        assert_eq!(
            summary,
            [
                ("shop-prod-token", true, 1),
                ("projects/other/secrets/shared-key", true, 0),
                ("shop-prod-database-url", true, 0),
                ("shop-prod-token-v2", false, 0),
            ],
            "what needs a restart first, then bound, then the rest"
        );
        assert!(states[1].secret.versions.is_err(), "another project's secret we may not read");
        assert!(!states[1].secret.own && states[0].secret.own);
        assert!(matches!(states[0].findings[0], Finding::NotLoaded { version: 2, .. }));

        // With `apps: run`, only what the service reads.
        let secrets = load(&gcp, AppsConvention::Run, &service).unwrap();
        assert_eq!(secrets.len(), 3);
    }
}
