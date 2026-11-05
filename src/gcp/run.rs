//! Cloud Run Admin API v2: services.

use serde::Deserialize;

use super::{Gcp, Opts, Result};
use crate::util::{parse_rfc3339, uri_encode};

/// A Cloud Run service, with what the Apps list shows.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Service {
    /// `projects/{project}/locations/{region}/services/{id}`
    pub name: String,
    /// `https://{id}-{hash}.{region}.run.app`
    pub uri: String,
    /// `projects/…/revisions/{revision}`; empty until a revision is ready.
    pub latest_ready_revision: String,
    pub update_time: String,
    /// Who (or which service account) changed it last.
    pub last_modifier: String,
    pub template: Template,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Template {
    pub scaling: Scaling,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Scaling {
    /// Absent means 0.
    pub min_instance_count: Option<u32>,
    /// Absent means Cloud Run's default (100).
    pub max_instance_count: Option<u32>,
}

/// The last segment of a resource name.
fn last_segment(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

impl Service {
    /// The service id (`orders`).
    pub fn id(&self) -> &str {
        last_segment(&self.name)
    }

    /// Its region, from its name (lists can span every region).
    pub fn region(&self) -> &str {
        self.name.split('/').nth(3).unwrap_or_default()
    }

    /// The latest ready revision's id (`orders-00042-xyz`), or empty.
    pub fn revision(&self) -> &str {
        last_segment(&self.latest_ready_revision)
    }

    /// When it last changed (Unix seconds).
    pub fn updated(&self) -> Option<f64> {
        parse_rfc3339(&self.update_time)
    }

    pub fn min_instances(&self) -> u32 {
        self.template.scaling.min_instance_count.unwrap_or(0)
    }

    pub fn max_instances(&self) -> Option<u32> {
        self.template.scaling.max_instance_count
    }

    /// The service's page in the Cloud console.
    pub fn console_url(&self, project: &str) -> String {
        super::console_url(project, &format!("run/detail/{}/{}", self.region(), self.id()))
    }
}

/// The services of the client's project and region (every region when it has none).
fn services_url(gcp: &Gcp) -> String {
    let location = if gcp.region.is_empty() { "-" } else { gcp.region.as_str() };
    format!(
        "https://run.googleapis.com/v2/projects/{}/locations/{}/services",
        uri_encode(&gcp.project),
        uri_encode(location)
    )
}

const PAGE: &[(&str, &str)] = &[("pageSize", "100")];

/// Lists the services a page at a time, for a list that fills as it loads; `each` returns
/// false to stop.
pub fn list_services(gcp: &Gcp, each: impl FnMut(Vec<Service>) -> bool) -> Result<()> {
    gcp.pages(&services_url(gcp), PAGE, "services", Opts::default(), each)
}

/// Every service, sorted by id.
pub fn all_services(gcp: &Gcp) -> Result<Vec<Service>> {
    let mut all: Vec<Service> = gcp.list(&services_url(gcp), PAGE, "services", Opts::default())?;
    all.sort_by(|a, b| a.id().cmp(b.id()));
    Ok(all)
}
