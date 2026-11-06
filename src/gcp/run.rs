//! Cloud Run Admin API v2: services, their revisions, and the two changes gcli makes to a
//! service (a new revision with the same configuration, and the service-level minimum of
//! instances). Changes are long-running operations, followed until they are done.
//!
//! Plain environment variable values are never deserialized: only their names, and for
//! variables read from Secret Manager, which secret and version.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};

use super::{Gcp, GcpError, Opts, Result};
use crate::util::{parse_rfc3339, uri_encode};

/// A Cloud Run service: what the Apps list and an app's detail show.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Service {
    /// `projects/{project}/locations/{region}/services/{id}`
    pub name: String,
    /// `https://{id}-{hash}.{region}.run.app`
    pub uri: String,
    /// `projects/…/revisions/{revision}`; empty until a revision is ready.
    pub latest_ready_revision: String,
    /// Differs from the ready one while a revision deploys, or when the last one failed.
    pub latest_created_revision: String,
    pub update_time: String,
    /// Who (or which service account) changed it last.
    pub last_modifier: String,
    /// `INGRESS_TRAFFIC_ALL`, `INGRESS_TRAFFIC_INTERNAL_LOAD_BALANCER`…
    pub ingress: String,
    /// The `run.app` URL does not resolve.
    pub default_uri_disabled: bool,
    /// Nobody's `run.routes.invoke` is checked: anyone who reaches it may call it.
    pub invoker_iam_disabled: bool,
    /// IAP on the service itself, in front of its `run.app` URL too.
    pub iap_enabled: bool,
    pub template: Template,
    /// Service-level scaling: it applies to every revision and changes without a new one.
    pub scaling: ServiceScaling,
    /// The traffic as configured (`traffic`) and as it is (`trafficStatuses`).
    pub traffic: Vec<TrafficTarget>,
    pub traffic_statuses: Vec<TrafficTarget>,
    pub conditions: Vec<Condition>,
    pub terminal_condition: Condition,
}

/// What each new revision is made from.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Template {
    pub scaling: Scaling,
    /// `300s`: how long a request may run before Cloud Run ends it.
    pub timeout: String,
    pub containers: Vec<Container>,
    pub volumes: Vec<Volume>,
    pub max_instance_request_concurrency: Option<u32>,
    pub service_account: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Scaling {
    /// Absent means 0.
    pub min_instance_count: Option<u32>,
    /// Absent means Cloud Run's default (100).
    pub max_instance_count: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ServiceScaling {
    pub min_instance_count: Option<u32>,
    /// `MANUAL` keeps `manualInstanceCount` instances, whatever the traffic.
    pub scaling_mode: String,
    pub manual_instance_count: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Container {
    pub image: String,
    pub env: Vec<EnvVar>,
    pub resources: Resources,
    pub volume_mounts: Vec<VolumeMount>,
}

/// An environment variable's name and, when it comes from Secret Manager, which secret. A
/// plain value is left unread on purpose.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EnvVar {
    pub name: String,
    pub value_source: Option<ValueSource>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ValueSource {
    pub secret_key_ref: Option<SecretKeyRef>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SecretKeyRef {
    /// A secret id of the service's project, or `projects/{p}/secrets/{id}`.
    pub secret: String,
    /// `latest` or a version number.
    pub version: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Resources {
    /// `cpu`, `memory`.
    pub limits: BTreeMap<String, String>,
    /// True: CPU only while a request runs (request-based billing). Absent once resources are
    /// set means false: CPU always allocated (instance-based billing).
    pub cpu_idle: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Volume {
    pub name: String,
    pub secret: Option<SecretVolume>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SecretVolume {
    pub secret: String,
    /// Empty: one file named after the secret, with its latest version.
    pub items: Vec<SecretItem>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SecretItem {
    pub path: String,
    pub version: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct VolumeMount {
    pub name: String,
    pub mount_path: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TrafficTarget {
    /// `TRAFFIC_TARGET_ALLOCATION_TYPE_LATEST` (follows new revisions) or `…_REVISION`.
    #[serde(rename = "type")]
    pub kind: String,
    pub revision: String,
    pub percent: u32,
}

impl TrafficTarget {
    pub fn follows_latest(&self) -> bool {
        self.kind.ends_with("_LATEST")
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Condition {
    #[serde(rename = "type")]
    pub kind: String,
    /// `CONDITION_SUCCEEDED`, `CONDITION_FAILED`, `CONDITION_PENDING`…
    pub state: String,
    pub message: String,
    pub last_transition_time: String,
}

impl Condition {
    pub fn succeeded(&self) -> bool {
        self.state == "CONDITION_SUCCEEDED"
    }
}

/// A revision: an immutable snapshot of the template, with who made it and the image digest
/// it runs.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Revision {
    pub name: String,
    pub create_time: String,
    pub creator: String,
    pub labels: BTreeMap<String, String>,
    pub containers: Vec<Container>,
    pub volumes: Vec<Volume>,
    pub container_statuses: Vec<ContainerStatus>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ContainerStatus {
    /// `…/image@sha256:…`
    pub image_digest: String,
}

/// A secret a revision reads: as an environment variable (resolved once, when an instance
/// starts) or as a mounted file (read from Secret Manager on every access).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRef {
    /// The variable's name, or the file's path for a mount.
    pub target: String,
    /// As written in the service: an id, or `projects/{p}/secrets/{id}`.
    pub secret: String,
    /// `latest` or a version number.
    pub version: String,
    pub mounted: bool,
}

impl SecretRef {
    /// The secret's id, without its project.
    pub fn secret_id(&self) -> &str {
        last_segment(&self.secret)
    }
}

/// The secrets read by `containers` (variables) and `volumes` (mounted files).
pub fn secret_refs(containers: &[Container], volumes: &[Volume]) -> Vec<SecretRef> {
    let version = |v: &str| if v.is_empty() { "latest".to_string() } else { v.to_string() };
    let mut refs = Vec::new();
    for c in containers {
        for e in &c.env {
            if let Some(r) = e.value_source.as_ref().and_then(|v| v.secret_key_ref.as_ref()) {
                refs.push(SecretRef {
                    target: e.name.clone(),
                    secret: r.secret.clone(),
                    version: version(&r.version),
                    mounted: false,
                });
            }
        }
        for m in &c.volume_mounts {
            let Some(s) = volumes.iter().find(|v| v.name == m.name).and_then(|v| v.secret.as_ref()) else { continue };
            let base = m.mount_path.trim_end_matches('/');
            if s.items.is_empty() {
                let target = format!("{base}/{}", last_segment(&s.secret));
                refs.push(SecretRef { target, secret: s.secret.clone(), version: "latest".into(), mounted: true });
            }
            for item in &s.items {
                let target = format!("{base}/{}", item.path);
                refs.push(SecretRef {
                    target,
                    secret: s.secret.clone(),
                    version: version(&item.version),
                    mounted: true,
                });
            }
        }
    }
    refs
}

/// Names of the plain (not secret) variables of `containers`.
pub fn plain_env_names(containers: &[Container]) -> Vec<String> {
    containers
        .iter()
        .flat_map(|c| c.env.iter())
        .filter(|e| e.value_source.as_ref().is_none_or(|v| v.secret_key_ref.is_none()))
        .map(|e| e.name.clone())
        .collect()
}

/// The last segment of a resource name.
pub fn last_segment(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

/// Request timeout Cloud Run applies when a service sets none.
pub const DEFAULT_TIMEOUT_SECS: f64 = 300.0;

/// `300s`, `1.5s` → seconds.
fn duration_secs(text: &str) -> Option<f64> {
    text.trim().strip_suffix('s')?.parse().ok()
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

    /// A revision id without the service's name in front (`00042-xyz`), for narrow columns.
    pub fn short<'a>(&self, revision: &'a str) -> &'a str {
        revision.strip_prefix(self.id()).and_then(|r| r.strip_prefix('-')).unwrap_or(revision)
    }

    /// The newest revision failed (or is still deploying): what serves is older.
    pub fn latest_not_ready(&self) -> bool {
        !self.latest_created_revision.is_empty() && self.latest_created_revision != self.latest_ready_revision
    }

    /// When it last changed (Unix seconds).
    pub fn updated(&self) -> Option<f64> {
        parse_rfc3339(&self.update_time)
    }

    /// About when the latest ready revision was made. The list has no revision times, and
    /// `updateTime` also moves for changes that make none (traffic, ingress, service-level
    /// scaling); `ConfigurationsReady` last turns true when a new revision becomes ready,
    /// within seconds of its creation.
    pub fn revision_ready_at(&self) -> Option<f64> {
        self.conditions
            .iter()
            .find(|c| c.kind == "ConfigurationsReady" && c.succeeded())
            .and_then(|c| parse_rfc3339(&c.last_transition_time))
            .or_else(|| self.updated())
    }

    /// Instances kept warm: the higher of the service's and the revision's minimum (Cloud
    /// Run's autoscaler honours the highest).
    pub fn min_instances(&self) -> u32 {
        self.service_min().max(self.revision_min())
    }

    /// The service-level minimum (what `m` changes).
    pub fn service_min(&self) -> u32 {
        self.scaling.min_instance_count.unwrap_or(0)
    }

    /// The minimum the revision template sets (changed by a deploy).
    pub fn revision_min(&self) -> u32 {
        self.template.scaling.min_instance_count.unwrap_or(0)
    }

    pub fn max_instances(&self) -> Option<u32> {
        self.template.scaling.max_instance_count
    }

    /// Manual scaling: a fixed number of instances.
    pub fn manual_instances(&self) -> Option<u32> {
        (self.scaling.scaling_mode == "MANUAL").then(|| self.scaling.manual_instance_count.unwrap_or(0))
    }

    /// CPU allocated all the time (instance-based billing) rather than only during requests.
    pub fn cpu_always_on(&self) -> bool {
        self.template.containers.iter().any(|c| match c.resources.cpu_idle {
            Some(idle) => !idle,
            None => !c.resources.limits.is_empty(),
        })
    }

    /// `all`, `internal`, `internal-load-balancer`.
    pub fn ingress_label(&self) -> String {
        self.ingress.strip_prefix("INGRESS_TRAFFIC_").unwrap_or(&self.ingress).to_lowercase().replace('_', "-")
    }

    /// The request timeout, in seconds.
    pub fn timeout_secs(&self) -> f64 {
        duration_secs(&self.template.timeout).filter(|t| *t > 0.0).unwrap_or(DEFAULT_TIMEOUT_SECS)
    }

    /// The traffic as it is, unless it all goes to the latest revision:
    /// `90% latest · 10% 00041-abc`.
    pub fn traffic_split(&self) -> Option<String> {
        let targets = if self.traffic_statuses.is_empty() { &self.traffic } else { &self.traffic_statuses };
        let all_latest = targets.iter().all(|t| t.follows_latest() || t.percent == 0);
        if targets.is_empty() || all_latest {
            return None;
        }
        let parts: Vec<String> = targets
            .iter()
            .filter(|t| t.percent > 0)
            .map(|t| {
                if t.follows_latest() || t.revision.is_empty() {
                    format!("{}% latest", t.percent)
                } else {
                    format!("{}% {}", t.percent, self.short(&t.revision))
                }
            })
            .collect();
        Some(parts.join(" · "))
    }

    /// The share of traffic that does not follow new revisions: a restart leaves it where it is.
    pub fn pinned_percent(&self) -> u32 {
        self.traffic.iter().filter(|t| !t.follows_latest()).map(|t| t.percent).sum()
    }

    /// The secrets the template reads.
    pub fn secret_refs(&self) -> Vec<SecretRef> {
        secret_refs(&self.template.containers, &self.template.volumes)
    }

    /// The service's page in the Cloud console.
    pub fn console_url(&self, project: &str) -> String {
        super::console_url(project, &format!("run/detail/{}/{}", self.region(), self.id()))
    }
}

impl Revision {
    pub fn id(&self) -> &str {
        last_segment(&self.name)
    }

    pub fn created(&self) -> Option<f64> {
        parse_rfc3339(&self.create_time)
    }

    /// `sha256:0123456789ab` from the first container's status, else the image's last part.
    pub fn image_short(&self) -> String {
        if let Some((_, digest)) = self.container_statuses.iter().find_map(|s| s.image_digest.split_once('@')) {
            return digest.chars().take(19).collect();
        }
        last_segment(self.containers.first().map(|c| c.image.as_str()).unwrap_or_default()).to_string()
    }

    pub fn secret_refs(&self) -> Vec<SecretRef> {
        secret_refs(&self.containers, &self.volumes)
    }

    pub fn plain_env_names(&self) -> Vec<String> {
        plain_env_names(&self.containers)
    }
}

/// The location services are listed in: the client's region, or every region (`-`).
fn location(gcp: &Gcp) -> &str {
    if gcp.region.is_empty() { "-" } else { gcp.region.as_str() }
}

/// The services of the client's project and region (every region when it has none).
fn services_url(gcp: &Gcp) -> String {
    format!(
        "https://run.googleapis.com/v2/projects/{}/locations/{}/services",
        uri_encode(&gcp.project),
        uri_encode(location(gcp))
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

/// What `locations/-` answers: the services of every region at once, and the regions it could not
/// ask. Those include, always, regions the project may not use.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct EveryRegion {
    pub services: Vec<Service>,
    pub unreachable: Vec<String>,
}

/// Every region's services at once. A wildcard has no pages: `pageSize` cuts its answer, with no
/// token to go on, so asking for 100 when there are more gives 100 and says nothing. Asked for none
/// it has given them all, but it cannot tell when it left some out: for a whole list, read each
/// region it names with [`in_region`].
pub fn every_region(gcp: &Gcp) -> Result<EveryRegion> {
    gcp.get(&format!("https://run.googleapis.com/v2/projects/{}/locations/-/services", uri_encode(&gcp.project)), &[])
}

/// The services of `region`, page by page.
pub fn in_region(gcp: &Gcp, region: &str) -> Result<Vec<Service>> {
    let url = format!(
        "https://run.googleapis.com/v2/projects/{}/locations/{}/services",
        uri_encode(&gcp.project),
        uri_encode(region)
    );
    gcp.list(&url, PAGE, "services", Opts::default())
}

/// An IAM policy, as who holds which role.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct IamPolicy {
    pub bindings: Vec<IamBinding>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct IamBinding {
    pub role: String,
    pub members: Vec<String>,
}

/// Who holds which role on `service` (its resource name).
pub fn iam_policy(gcp: &Gcp, service: &str) -> Result<IamPolicy> {
    gcp.get(&format!("https://run.googleapis.com/v2/{service}:getIamPolicy"), &[])
}

/// The service `id`: read directly in the client's region, or found among every region's.
pub fn find_service(gcp: &Gcp, id: &str) -> Result<Service> {
    if gcp.region.is_empty() {
        return all_services(gcp)?.into_iter().find(|s| s.id() == id).ok_or_else(|| GcpError::Api {
            status: 404,
            code: "NOT_FOUND".into(),
            message: format!("no Cloud Run service '{id}' in {}", gcp.project),
        });
    }
    gcp.get(&format!("{}/{}", services_url(gcp), uri_encode(id)), &[])
}

/// The newest revisions of `service` (its resource name), newest first, at most `limit`.
pub fn recent_revisions(gcp: &Gcp, service: &str, limit: usize) -> Result<Vec<Revision>> {
    let url = format!("https://run.googleapis.com/v2/{service}/revisions");
    let size = limit.to_string();
    let mut out = Vec::new();
    gcp.pages(&url, &[("pageSize", size.as_str())], "revisions", Opts::default(), |page: Vec<Revision>| {
        out.extend(page);
        out.len() < limit
    })?;
    out.sort_by(|a, b| b.create_time.cmp(&a.create_time));
    out.truncate(limit);
    Ok(out)
}

/// The template label a restart sets: its value (Unix seconds) makes the template differ, so
/// Cloud Run makes a revision, and tells later who restarted and when.
pub const RESTART_LABEL: &str = "gcli-restart";

/// What a restart sends: the service exactly as read, with [`RESTART_LABEL`] set on its
/// template. A revision name pinned in the template is dropped, since a second revision
/// cannot take the same name.
pub fn restart_body(service: &Value, now: u64) -> std::result::Result<Value, String> {
    let mut body = service.clone();
    let template = body
        .get_mut("template")
        .and_then(Value::as_object_mut)
        .ok_or("the service has no template to make a revision from")?;
    template.remove("revision");
    let labels = template.entry("labels").or_insert_with(|| json!({}));
    let labels = labels.as_object_mut().ok_or("the template's labels are not a map")?;
    labels.insert(RESTART_LABEL.into(), json!(now.to_string()));
    Ok(body)
}

/// What setting the service-level minimum sends (with `updateMask=scaling.minInstanceCount`).
pub fn min_instances_body(n: u32) -> Value {
    json!({ "scaling": { "minInstanceCount": n } })
}

/// Restarts `service` (its resource name): reads it as it is, sends it back with a new
/// [`RESTART_LABEL`] and its etag (so a change made meanwhile is refused rather than
/// overwritten), and waits for the new revision. Returns its id.
pub fn restart(gcp: &Gcp, service: &str, now: u64) -> Result<String> {
    let url = format!("https://run.googleapis.com/v2/{service}");
    // Exact: the body goes back, and every field (env values included) must stay as it is.
    let current = gcp.get_exact(&url)?;
    let body = restart_body(&current, now).map_err(|e| GcpError::Api {
        status: 400,
        code: "FAILED_PRECONDITION".into(),
        message: e,
    })?;
    // Not sent again after a lost connection: the first may have made the revision.
    let op: Operation = gcp.patch(&url, &[], &body, Opts::default())?;
    let svc: Service = serde_json::from_value(wait(gcp, op)?).unwrap_or_default();
    let revision =
        if svc.latest_created_revision.is_empty() { svc.latest_ready_revision } else { svc.latest_created_revision };
    Ok(last_segment(&revision).to_string())
}

/// Sets the service-level minimum of instances of `service` (its resource name). It applies
/// to the running revision at once and makes no new one.
pub fn set_min_instances(gcp: &Gcp, service: &str, n: u32) -> Result<Service> {
    let url = format!("https://run.googleapis.com/v2/{service}");
    // The same value set twice does no harm: safe to send again after a lost connection.
    let opts = Opts { idempotent: true, ..Opts::default() };
    let op: Operation = gcp.patch(&url, &[("updateMask", "scaling.minInstanceCount")], &min_instances_body(n), opts)?;
    Ok(serde_json::from_value(wait(gcp, op)?).unwrap_or_default())
}

/// A long-running operation.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Operation {
    name: String,
    done: bool,
    error: Option<OperationError>,
    response: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct OperationError {
    code: i32,
    message: String,
}

/// Between two reads of an operation.
const POLL: Duration = Duration::from_millis(if cfg!(test) { 1 } else { 2000 });
/// A revision that takes longer than this to start is left to the console.
const OPERATION_LIMIT: Duration = Duration::from_secs(20 * 60);

/// Follows `op` until it is done: its response (the service), or its error.
fn wait(gcp: &Gcp, mut op: Operation) -> Result<Value> {
    let since = Instant::now();
    loop {
        if op.done {
            return match op.error {
                Some(e) => Err(GcpError::Api {
                    status: 400,
                    code: format!("OPERATION_FAILED({})", e.code),
                    message: e.message,
                }),
                None => Ok(op.response.unwrap_or_default()),
            };
        }
        if op.name.is_empty() || since.elapsed() > OPERATION_LIMIT {
            return Err(GcpError::Network(format!(
                "Cloud Run is still applying the change after {} min; see the console",
                since.elapsed().as_secs() / 60
            )));
        }
        std::thread::sleep(POLL);
        op = gcp.get(&format!("https://run.googleapis.com/v2/{}", op.name), &[])?;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::gcp::mock::{MockServer, Reply};

    fn client(server: &MockServer) -> std::sync::Arc<Gcp> {
        Gcp::with("p", "us-east1", crate::gcp::auth::tests::fixed("ya29.t"), Some(server.url()))
    }

    #[test]
    fn lists_services_across_pages() {
        let server = MockServer::start(vec![
            Reply::json(
                200,
                r#"{"services":[{"name":"projects/p/locations/us-east1/services/orders","uri":"https://orders-abc-ue.a.run.app",
                    "latestReadyRevision":"projects/p/locations/us-east1/services/orders/revisions/orders-00042-xyz",
                    "updateTime":"2026-10-14T17:55:03.123456Z","lastModifier":"deployer@p.iam.gserviceaccount.com",
                    "template":{"scaling":{"minInstanceCount":1,"maxInstanceCount":4}}}],"nextPageToken":"n2"}"#,
            ),
            Reply::json(200, r#"{"services":[{"name":"projects/p/locations/us-east1/services/api","template":{}}]}"#),
        ]);
        let services = all_services(&client(&server)).unwrap();
        assert_eq!(services.iter().map(Service::id).collect::<Vec<_>>(), ["api", "orders"]);
        let orders = &services[1];
        assert_eq!((orders.region(), orders.revision()), ("us-east1", "orders-00042-xyz"));
        assert_eq!(orders.short(orders.revision()), "00042-xyz");
        assert_eq!((orders.min_instances(), orders.max_instances()), (1, Some(4)));
        assert_eq!(orders.updated().map(|t| t as i64), Some(1_792_000_503));
        assert_eq!(orders.console_url("p"), "https://console.cloud.google.com/run/detail/us-east1/orders?project=p");
        let api = &services[0];
        assert_eq!((api.min_instances(), api.max_instances(), api.revision(), api.updated()), (0, None, "", None));
        assert_eq!(api.timeout_secs(), DEFAULT_TIMEOUT_SECS);
        let paths: Vec<String> = server.requests().iter().map(|r| r.path.clone()).collect();
        assert_eq!(
            paths,
            [
                "/run/v2/projects/p/locations/us-east1/services?pageSize=100",
                "/run/v2/projects/p/locations/us-east1/services?pageSize=100&pageToken=n2"
            ]
        );
    }

    #[test]
    fn no_region_lists_every_region() {
        let server = MockServer::start(vec![Reply::json(200, "{}")]);
        let gcp = Gcp::with("p", "", crate::gcp::auth::tests::fixed("t"), Some(server.url()));
        assert!(all_services(&gcp).unwrap().is_empty());
        assert_eq!(server.requests()[0].path, "/run/v2/projects/p/locations/-/services?pageSize=100");
    }

    /// A service with a bit of everything: a pinned revision name, labels, a service-level
    /// minimum, split traffic, secrets as variables and as a mounted file.
    pub(crate) const SERVICE: &str = r#"{
        "name":"projects/p/locations/us-east1/services/shop","uid":"u-1","generation":"12",
        "labels":{"team":"web"},"etag":"\"etag-12\"","ingress":"INGRESS_TRAFFIC_INTERNAL_LOAD_BALANCER",
        "template":{
            "revision":"shop-pinned-name",
            "labels":{"secrets-reloaded":"20261105"},
            "scaling":{"maxInstanceCount":10,"minInstanceCount":1},
            "timeout":"600s","maxInstanceRequestConcurrency":80,"serviceAccount":"run@p.iam.gserviceaccount.com",
            "containers":[{"image":"us-docker.pkg.dev/p/r/shop:abc","resources":{"limits":{"cpu":"1","memory":"512Mi"},"cpuIdle":true},
                "env":[{"name":"NODE_ENV","value":"production\r\nwith a carriage return"},
                       {"name":"DATABASE_URL","valueSource":{"secretKeyRef":{"secret":"shop-prod-database-url","version":"latest"}}},
                       {"name":"API_KEY","valueSource":{"secretKeyRef":{"secret":"projects/other/secrets/shared-key","version":"3"}}}],
                "volumeMounts":[{"name":"creds","mountPath":"/secrets/"}]}],
            "volumes":[{"name":"creds","secret":{"secret":"shop-prod-sa-json","items":[{"path":"sa.json","version":"2"}]}}]
        },
        "scaling":{"minInstanceCount":2},
        "traffic":[{"type":"TRAFFIC_TARGET_ALLOCATION_TYPE_LATEST","percent":90},
                   {"type":"TRAFFIC_TARGET_ALLOCATION_TYPE_REVISION","revision":"shop-00011-abc","percent":10}],
        "trafficStatuses":[{"type":"TRAFFIC_TARGET_ALLOCATION_TYPE_LATEST","revision":"shop-00012-def","percent":90},
                   {"type":"TRAFFIC_TARGET_ALLOCATION_TYPE_REVISION","revision":"shop-00011-abc","percent":10}],
        "conditions":[{"type":"ConfigurationsReady","state":"CONDITION_SUCCEEDED","lastTransitionTime":"2026-11-01T10:00:05Z"}],
        "latestReadyRevision":"projects/p/locations/us-east1/services/shop/revisions/shop-00012-def",
        "latestCreatedRevision":"projects/p/locations/us-east1/services/shop/revisions/shop-00012-def",
        "updateTime":"2026-11-03T08:00:00Z","uri":"https://shop-x.a.run.app"
    }"#;

    #[test]
    fn reads_what_the_detail_shows() {
        let s: Service = serde_json::from_str(SERVICE).unwrap();
        assert_eq!((s.service_min(), s.revision_min(), s.min_instances()), (2, 1, 2));
        assert_eq!(s.timeout_secs(), 600.0);
        assert_eq!(s.ingress_label(), "internal-load-balancer");
        assert!(!s.cpu_always_on());
        assert_eq!(s.traffic_split().as_deref(), Some("90% latest · 10% 00011-abc"));
        assert_eq!(s.pinned_percent(), 10);
        // Made by its last revision, not by the later service-level change.
        assert_eq!(s.revision_ready_at(), parse_rfc3339("2026-11-01T10:00:05Z"));
        assert!(!s.latest_not_ready());
        let refs = s.secret_refs();
        let shown: Vec<(&str, &str, &str, bool)> =
            refs.iter().map(|r| (r.target.as_str(), r.secret_id(), r.version.as_str(), r.mounted)).collect();
        assert_eq!(
            shown,
            [
                ("DATABASE_URL", "shop-prod-database-url", "latest", false),
                ("API_KEY", "shared-key", "3", false),
                ("/secrets/sa.json", "shop-prod-sa-json", "2", true),
            ]
        );
        assert_eq!(plain_env_names(&s.template.containers), ["NODE_ENV"]);
        // No cpuIdle once resources are set: always-on.
        let always: Service = serde_json::from_str(
            r#"{"template":{"containers":[{"resources":{"limits":{"cpu":"1"}}}]},
                "traffic":[{"type":"TRAFFIC_TARGET_ALLOCATION_TYPE_LATEST","percent":100}]}"#,
        )
        .unwrap();
        assert!(always.cpu_always_on());
        assert_eq!(always.traffic_split(), None);
    }

    #[test]
    fn a_restart_sends_the_service_back_with_one_label_and_its_etag() {
        let server = MockServer::start(vec![
            Reply::json(200, SERVICE),
            Reply::json(200, r#"{"name":"projects/p/locations/us-east1/operations/op-1","done":false}"#),
            Reply::json(200, r#"{"name":"projects/p/locations/us-east1/operations/op-1","done":false}"#),
            Reply::json(
                200,
                r#"{"name":"projects/p/locations/us-east1/operations/op-1","done":true,"response":{
                    "name":"projects/p/locations/us-east1/services/shop",
                    "latestCreatedRevision":"projects/p/locations/us-east1/services/shop/revisions/shop-00013-new"}}"#,
            ),
        ]);
        let revision = restart(&client(&server), "projects/p/locations/us-east1/services/shop", 1_790_000_000).unwrap();
        assert_eq!(revision, "shop-00013-new");
        let requests = server.requests();
        let calls: Vec<(&str, &str)> = requests.iter().map(|r| (r.method.as_str(), r.path.as_str())).collect();
        assert_eq!(
            calls,
            [
                ("GET", "/run/v2/projects/p/locations/us-east1/services/shop"),
                ("PATCH", "/run/v2/projects/p/locations/us-east1/services/shop"),
                ("GET", "/run/v2/projects/p/locations/us-east1/operations/op-1"),
                ("GET", "/run/v2/projects/p/locations/us-east1/operations/op-1"),
            ]
        );
        let sent: Value = serde_json::from_str(&requests[1].body).unwrap();
        let mut expected: Value = serde_json::from_str(SERVICE).unwrap();
        expected["template"].as_object_mut().unwrap().remove("revision");
        expected["template"]["labels"]["gcli-restart"] = json!("1790000000");
        assert_eq!(sent, expected, "only the label changes (and a pinned revision name goes)");
        assert_eq!(sent["etag"], "\"etag-12\"");
        assert_eq!(sent["template"]["labels"]["secrets-reloaded"], "20261105");
        // Values the display would clean are sent back byte for byte.
        assert_eq!(sent["template"]["containers"][0]["env"][0]["value"], "production\r\nwith a carriage return");
        // In the same order, so a diff of the service shows only the label.
        let keys: Vec<&str> = sent.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys[..3], ["name", "uid", "generation"]);
    }

    #[test]
    fn a_template_without_labels_gets_them() {
        let body = restart_body(&json!({"template": {"containers": []}, "etag": "e"}), 7).unwrap();
        assert_eq!(body, json!({"template": {"containers": [], "labels": {"gcli-restart": "7"}}, "etag": "e"}));
        assert!(restart_body(&json!({"name": "x"}), 7).is_err());
    }

    #[test]
    fn a_failed_operation_says_why() {
        let server = MockServer::start(vec![
            Reply::json(200, SERVICE),
            Reply::json(
                200,
                r#"{"name":"op","done":true,"error":{"code":9,"message":"Revision 'shop-00013-new' is not ready: container failed to start"}}"#,
            ),
        ]);
        let e = restart(&client(&server), "projects/p/locations/us-east1/services/shop", 1).unwrap_err();
        assert!(e.short().contains("container failed to start"), "{e}");
        // A change made meanwhile (another etag) is refused, and not sent again.
        let server = MockServer::start(vec![
            Reply::json(200, SERVICE),
            Reply::json(409, r#"{"error":{"code":409,"status":"ABORTED","message":"etag mismatch"}}"#),
        ]);
        let e = restart(&client(&server), "projects/p/locations/us-east1/services/shop", 1).unwrap_err();
        assert!(matches!(&e, GcpError::Api { code, .. } if code == "ABORTED"), "{e:?}");
        assert_eq!(server.requests().len(), 2);
    }

    #[test]
    fn min_instances_change_one_field_without_a_revision() {
        let server = MockServer::start(vec![Reply::json(
            200,
            r#"{"name":"op","done":true,"response":{"name":"projects/p/locations/us-east1/services/shop","scaling":{"minInstanceCount":3}}}"#,
        )]);
        let svc = set_min_instances(&client(&server), "projects/p/locations/us-east1/services/shop", 3).unwrap();
        assert_eq!(svc.service_min(), 3);
        let r = &server.requests()[0];
        assert_eq!(r.method, "PATCH");
        assert_eq!(r.path, "/run/v2/projects/p/locations/us-east1/services/shop?updateMask=scaling.minInstanceCount");
        assert_eq!(serde_json::from_str::<Value>(&r.body).unwrap(), json!({"scaling": {"minInstanceCount": 3}}));
    }

    #[test]
    fn revisions_newest_first_with_their_secrets() {
        let server = MockServer::start(vec![Reply::json(
            200,
            r#"{"revisions":[
                {"name":"projects/p/locations/us-east1/services/shop/revisions/shop-00011-abc","createTime":"2026-10-01T00:00:00Z",
                 "creator":"ci@p.iam.gserviceaccount.com","containers":[{"image":"us-docker.pkg.dev/p/r/shop:v1","env":[{"name":"A","value":"x"}]}]},
                {"name":"projects/p/locations/us-east1/services/shop/revisions/shop-00012-def","createTime":"2026-11-01T10:00:00Z",
                 "creator":"ana@example.com","labels":{"gcli-restart":"1"},
                 "containers":[{"image":"us-docker.pkg.dev/p/r/shop:v2",
                    "env":[{"name":"A","value":"x"},{"name":"S","valueSource":{"secretKeyRef":{"secret":"shop-prod-s"}}}]}],
                 "containerStatuses":[{"name":"shop-1","imageDigest":"us-docker.pkg.dev/p/r/shop@sha256:0123456789abcdef0123"}]}
            ],"nextPageToken":"more"}"#,
        )]);
        let revs = recent_revisions(&client(&server), "projects/p/locations/us-east1/services/shop", 2).unwrap();
        assert_eq!(revs.iter().map(Revision::id).collect::<Vec<_>>(), ["shop-00012-def", "shop-00011-abc"]);
        assert_eq!(revs[0].image_short(), "sha256:0123456789ab");
        assert_eq!(revs[1].image_short(), "shop:v1");
        assert_eq!(revs[0].secret_refs()[0].version, "latest", "no version means latest");
        assert_eq!(revs[0].plain_env_names(), ["A"]);
        assert_eq!(server.requests().len(), 1, "stops at the limit");
        assert_eq!(
            server.requests()[0].path,
            "/run/v2/projects/p/locations/us-east1/services/shop/revisions?pageSize=2"
        );
    }

    #[test]
    fn finds_a_service_in_any_region() {
        let server = MockServer::start(vec![Reply::json(
            200,
            r#"{"services":[{"name":"projects/p/locations/europe-west1/services/shop"}]}"#,
        )]);
        let gcp = Gcp::with("p", "", crate::gcp::auth::tests::fixed("t"), Some(server.url()));
        assert_eq!(find_service(&gcp, "shop").unwrap().region(), "europe-west1");
        server.push(Reply::json(200, "{}"));
        assert!(matches!(find_service(&gcp, "nope"), Err(GcpError::Api { status: 404, .. })));
    }
}
