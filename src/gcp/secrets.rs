//! Secret Manager API v1: secrets and their versions, metadata only. Values are never read:
//! nothing here calls `:access`.

use serde::Deserialize;

use super::{Gcp, Opts, Result};
use crate::util::{parse_rfc3339, uri_encode};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Secret {
    /// `projects/{project number}/secrets/{id}`
    pub name: String,
    pub create_time: String,
}

impl Secret {
    pub fn id(&self) -> &str {
        super::run::last_segment(&self.name)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Version {
    /// `projects/{number}/secrets/{id}/versions/{n}`
    pub name: String,
    pub create_time: String,
    /// `ENABLED`, `DISABLED` or `DESTROYED`.
    pub state: String,
    pub destroy_time: String,
}

impl Version {
    /// Its number (`3`).
    pub fn number(&self) -> u64 {
        super::run::last_segment(&self.name).parse().unwrap_or(0)
    }

    pub fn created(&self) -> Option<f64> {
        parse_rfc3339(&self.create_time)
    }

    pub fn enabled(&self) -> bool {
        self.state == "ENABLED"
    }
}

/// The resource name of a secret as a service refers to it: an id of `project`, or already
/// `projects/{p}/secrets/{id}`.
pub fn resource(project: &str, secret: &str) -> String {
    if secret.starts_with("projects/") {
        secret.to_string()
    } else {
        format!("projects/{}/secrets/{}", uri_encode(project), uri_encode(secret))
    }
}

/// The client's project's secrets whose id starts with `prefix`, sorted by id. The API's
/// `name:` filter matches anywhere in the name, so the prefix is checked again here.
pub fn with_prefix(gcp: &Gcp, prefix: &str) -> Result<Vec<Secret>> {
    let url = format!("https://secretmanager.googleapis.com/v1/projects/{}/secrets", uri_encode(&gcp.project));
    let filter = format!("name:{prefix}");
    let mut all: Vec<Secret> =
        gcp.list(&url, &[("filter", filter.as_str()), ("pageSize", "250")], "secrets", Opts::default())?;
    all.retain(|s| s.id().starts_with(prefix));
    all.sort_by(|a, b| a.id().cmp(b.id()));
    Ok(all)
}

/// The versions of `secret` (its resource name), newest first.
pub fn versions(gcp: &Gcp, secret: &str) -> Result<Vec<Version>> {
    let url = format!("https://secretmanager.googleapis.com/v1/{secret}/versions");
    let mut all: Vec<Version> = gcp.list(&url, &[("pageSize", "100")], "versions", Opts::default())?;
    all.sort_by_key(|v| std::cmp::Reverse(v.number()));
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gcp::mock::{MockServer, Reply};

    #[test]
    fn lists_an_apps_secrets_and_their_versions_without_values() {
        let server = MockServer::start(vec![
            Reply::json(
                200,
                r#"{"secrets":[
                    {"name":"projects/123/secrets/shop-prod-token","createTime":"2026-10-01T00:00:00Z"},
                    {"name":"projects/123/secrets/myshop-prod-other","createTime":"2026-10-01T00:00:00Z"},
                    {"name":"projects/123/secrets/shop-prod-db","createTime":"2026-09-01T00:00:00Z"}]}"#,
            ),
            Reply::json(
                200,
                r#"{"versions":[
                    {"name":"projects/123/secrets/shop-prod-token/versions/2","createTime":"2026-11-02T00:00:00Z","state":"ENABLED"},
                    {"name":"projects/123/secrets/shop-prod-token/versions/10","createTime":"2026-11-04T00:00:00Z","state":"ENABLED"},
                    {"name":"projects/123/secrets/shop-prod-token/versions/1","createTime":"2026-10-01T00:00:00Z","state":"DESTROYED",
                     "destroyTime":"2026-10-21T00:00:00.123456789Z"}],"totalSize":3}"#,
            ),
        ]);
        let gcp = Gcp::with("p", "us-east1", crate::gcp::auth::tests::fixed("t"), Some(server.url()));
        let secrets = with_prefix(&gcp, "shop-prod-").unwrap();
        assert_eq!(secrets.iter().map(Secret::id).collect::<Vec<_>>(), ["shop-prod-db", "shop-prod-token"]);
        let versions = versions(&gcp, &secrets[1].name).unwrap();
        assert_eq!(versions.iter().map(Version::number).collect::<Vec<_>>(), [10, 2, 1], "newest first, by number");
        assert!(versions[0].enabled() && !versions[2].enabled());
        let paths: Vec<String> = server.requests().iter().map(|r| r.path.clone()).collect();
        assert_eq!(
            paths,
            [
                "/secretmanager/v1/projects/p/secrets?filter=name%3Ashop-prod-&pageSize=250",
                "/secretmanager/v1/projects/123/secrets/shop-prod-token/versions?pageSize=100",
            ]
        );
        assert!(paths.iter().all(|p| !p.contains(":access") && !p.contains("%3Aaccess")));
        assert_eq!(resource("p", "shop-prod-db"), "projects/p/secrets/shop-prod-db");
        assert_eq!(resource("p", "projects/other/secrets/k"), "projects/other/secrets/k");
    }
}
