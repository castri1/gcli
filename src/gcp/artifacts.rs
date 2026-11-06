//! Artifact Registry API v1, read-only: repositories, their size and their cleanup policies.
//! A list leaves the policies out, so saying whether a repository has one takes a read of the
//! repository itself. Locations have no wildcard: `locations/-` is refused.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::{Gcp, Opts, Result};
use crate::util::uri_encode;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Repository {
    /// `projects/{p}/locations/{l}/repositories/{id}`
    pub name: String,
    /// `DOCKER`, `NPM`, `MAVEN`…
    pub format: String,
    /// `STANDARD_REPOSITORY`, `REMOTE_REPOSITORY`, `VIRTUAL_REPOSITORY`.
    pub mode: String,
    /// In bytes, written as a string.
    pub size_bytes: String,
    /// By id. Only a read of the repository holds them.
    pub cleanup_policies: BTreeMap<String, CleanupPolicy>,
    /// True: the policies are only tried, and delete nothing.
    pub cleanup_policy_dry_run: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CleanupPolicy {
    /// `DELETE` or `KEEP`.
    pub action: String,
}

impl Repository {
    pub fn id(&self) -> &str {
        super::run::last_segment(&self.name)
    }

    pub fn location(&self) -> &str {
        self.name.split('/').nth(3).unwrap_or_default()
    }

    pub fn size(&self) -> u64 {
        self.size_bytes.parse().unwrap_or(0)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Location {
    location_id: String,
}

fn project_url(gcp: &Gcp) -> String {
    format!("https://artifactregistry.googleapis.com/v1/projects/{}", uri_encode(&gcp.project))
}

/// The locations a repository can be in.
pub fn locations(gcp: &Gcp) -> Result<Vec<String>> {
    let url = format!("{}/locations", project_url(gcp));
    let all: Vec<Location> = gcp.list(&url, &[("pageSize", "200")], "locations", Opts::default())?;
    Ok(all.into_iter().map(|l| l.location_id).collect())
}

/// The repositories of a location, without their cleanup policies.
pub fn repositories(gcp: &Gcp, location: &str) -> Result<Vec<Repository>> {
    let url = format!("{}/locations/{}/repositories", project_url(gcp), uri_encode(location));
    gcp.list(&url, &[("pageSize", "1000")], "repositories", Opts::default())
}

/// A repository (its resource name), with its cleanup policies.
pub fn repository(gcp: &Gcp, name: &str) -> Result<Repository> {
    gcp.get(&format!("https://artifactregistry.googleapis.com/v1/{name}"), &[])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gcp::mock::{MockServer, Reply};

    #[test]
    fn a_list_leaves_the_policies_out_and_a_read_has_them() {
        let server = MockServer::start(vec![
            Reply::json(
                200,
                r#"{"repositories":[{"name":"projects/p/locations/us-east1/repositories/images","format":"DOCKER",
                    "mode":"STANDARD_REPOSITORY","sizeBytes":"123456789"}]}"#,
            ),
            Reply::json(
                200,
                r#"{"name":"projects/p/locations/us-east1/repositories/images","sizeBytes":"123456789",
                    "cleanupPolicies":{"old":{"id":"old","action":"DELETE","condition":{"olderThan":"259200s"}}}}"#,
            ),
            Reply::json(200, r#"{"locations":[{"locationId":"us"},{"locationId":"us-east1"}]}"#),
        ]);
        let gcp = Gcp::with("p", "us-east1", crate::gcp::auth::tests::fixed("t"), Some(server.url()));
        let listed = repositories(&gcp, "us-east1").unwrap();
        assert_eq!((listed[0].id(), listed[0].location(), listed[0].size()), ("images", "us-east1", 123_456_789));
        assert!(listed[0].cleanup_policies.is_empty());
        let read = repository(&gcp, &listed[0].name).unwrap();
        assert_eq!(read.cleanup_policies["old"].action, "DELETE");
        assert_eq!(locations(&gcp).unwrap(), ["us", "us-east1"]);
        let paths: Vec<String> = server.requests().iter().map(|r| r.path.clone()).collect();
        assert_eq!(
            paths,
            [
                "/artifactregistry/v1/projects/p/locations/us-east1/repositories?pageSize=1000",
                "/artifactregistry/v1/projects/p/locations/us-east1/repositories/images",
                "/artifactregistry/v1/projects/p/locations?pageSize=200",
            ]
        );
    }
}
