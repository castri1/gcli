//! Compute Engine API v1, read-only: what a load balancer is made of, and the quotas of the
//! project and its regions. Nothing here changes anything.
//!
//! Lists are the aggregated ones where they hold every scope, so a regional load balancer counts
//! too. Network endpoint groups are the exception: their aggregated list holds only zonal ones
//! and never a serverless NEG, so those are read region by region.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::{Gcp, GcpError, Opts, Result};
use crate::util::uri_encode;

/// A list's pages, at most: the same guard as [`Gcp::pages`] for what it cannot read.
const MAX_PAGES: usize = 100;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Quota {
    /// `GLOBAL_EXTERNAL_MANAGED_BACKEND_SERVICES`…
    pub metric: String,
    pub limit: f64,
    pub usage: f64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct Project {
    quotas: Vec<Quota>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Region {
    pub name: String,
    pub quotas: Vec<Quota>,
}

/// A backend service, global or regional.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BackendService {
    pub name: String,
    pub self_link: String,
    /// A regional one's region, as a URL.
    pub region: String,
    /// `EXTERNAL_MANAGED`, `EXTERNAL`, `INTERNAL_MANAGED`…
    pub load_balancing_scheme: String,
    /// `HTTP`, `HTTPS`, `HTTP2`, `TCP`, `SSL`…
    pub protocol: String,
    pub backends: Vec<Backend>,
    /// Only whether it is on: nothing else of its client is read.
    pub iap: Iap,
    /// A Cloud Armor policy, as a URL.
    pub security_policy: String,
    /// What points at it, as Compute Engine keeps it: URL maps, TCP and SSL proxies…
    pub used_by: Vec<UsedBy>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Backend {
    /// A NEG or an instance group, as a URL.
    pub group: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Iap {
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct UsedBy {
    pub reference: String,
}

impl BackendService {
    /// `us-east1`, or empty for a global one.
    pub fn region_name(&self) -> &str {
        super::run::last_segment(&self.region)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UrlMap {
    pub name: String,
    pub self_link: String,
    pub default_service: String,
    pub default_route_action: Value,
    pub host_rules: Vec<HostRule>,
    pub path_matchers: Vec<PathMatcher>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HostRule {
    pub hosts: Vec<String>,
    pub path_matcher: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PathMatcher {
    pub name: String,
    pub default_service: String,
    pub default_route_action: Value,
    pub path_rules: Vec<Value>,
    pub route_rules: Vec<Value>,
}

/// The services named anywhere in `value`: a rule's `service`, a route action's weighted
/// `backendService`, a mirror's.
fn named_services(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, v) in map {
                match (key.as_str(), v) {
                    ("service" | "backendService", Value::String(s)) => out.push(s.clone()),
                    _ => named_services(v, out),
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|v| named_services(v, out)),
        _ => {}
    }
}

impl PathMatcher {
    /// What its rules send to: backend services and backend buckets, as URLs.
    pub fn services(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.default_service.is_empty() {
            out.push(self.default_service.clone());
        }
        named_services(&self.default_route_action, &mut out);
        self.path_rules.iter().chain(&self.route_rules).for_each(|r| named_services(r, &mut out));
        out
    }
}

impl UrlMap {
    /// What it sends to, everywhere it says so: backend services and backend buckets, as URLs.
    pub fn services(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.default_service.is_empty() {
            out.push(self.default_service.clone());
        }
        named_services(&self.default_route_action, &mut out);
        self.path_matchers.iter().for_each(|p| out.extend(p.services()));
        out
    }

    pub fn path_rules(&self) -> usize {
        self.path_matchers.iter().map(|p| p.path_rules.len()).sum()
    }

    pub fn route_rules(&self) -> usize {
        self.path_matchers.iter().map(|p| p.route_rules.len()).sum()
    }
}

/// A target HTTP or HTTPS proxy: between a forwarding rule and a URL map.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TargetProxy {
    pub self_link: String,
    pub url_map: String,
    pub ssl_certificates: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ForwardingRule {
    /// A proxy (or a service attachment), as a URL.
    pub target: String,
    /// A passthrough rule's backend, as a URL.
    pub backend_service: String,
}

/// A network endpoint group; only serverless ones say what they reach.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NetworkEndpointGroup {
    pub name: String,
    pub self_link: String,
    /// Its region, as a URL.
    pub region: String,
    /// `SERVERLESS`, `PRIVATE_SERVICE_CONNECT`, `INTERNET_FQDN_PORT`…
    pub network_endpoint_type: String,
    pub cloud_run: CloudRun,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CloudRun {
    /// The service it reaches. Empty with a URL mask, which reaches whichever a request names.
    pub service: String,
    pub url_mask: String,
}

impl NetworkEndpointGroup {
    pub fn region_name(&self) -> &str {
        super::run::last_segment(&self.region)
    }
}

/// A certificate of Compute Engine's own (Certificate Manager keeps others).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SslCertificate {
    pub name: String,
    pub self_link: String,
    /// `MANAGED` or `SELF_MANAGED`.
    #[serde(rename = "type")]
    pub kind: String,
    pub managed: ManagedSsl,
    pub subject_alternative_names: Vec<String>,
    pub expire_time: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ManagedSsl {
    /// `ACTIVE`, `PROVISIONING`, `PROVISIONING_FAILED`, `RENEWAL_FAILED`…
    pub status: String,
    pub domains: Vec<String>,
    /// Each domain and how it stands (`ACTIVE`, `FAILED_NOT_VISIBLE`…).
    pub domain_status: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SecurityPolicy {
    pub name: String,
}

fn base(gcp: &Gcp) -> String {
    format!("https://compute.googleapis.com/compute/v1/projects/{}", uri_encode(&gcp.project))
}

/// The project's quotas. Which metric counts what is not obvious: see `infra`.
pub fn project_quotas(gcp: &Gcp) -> Result<Vec<Quota>> {
    Ok(gcp.get::<Project>(&base(gcp), &[])?.quotas)
}

/// A region with its quotas: one Compute Engine does not know has none.
pub fn region(gcp: &Gcp, name: &str) -> Result<Region> {
    match gcp.get(&format!("{}/regions/{}", base(gcp), uri_encode(name)), &[]) {
        Err(GcpError::Api { status: 404, .. }) => Ok(Region { name: name.to_string(), quotas: Vec::new() }),
        other => other,
    }
}

/// Every region with its quotas (one call).
pub fn regions(gcp: &Gcp) -> Result<Vec<Region>> {
    gcp.list(&format!("{}/regions", base(gcp)), &[], "items", Opts::default())
}

/// The NEGs of a region: a region Compute Engine does not know has none.
pub fn negs(gcp: &Gcp, region: &str) -> Result<Vec<NetworkEndpointGroup>> {
    let url = format!("{}/regions/{}/networkEndpointGroups", base(gcp), uri_encode(region));
    match gcp.list(&url, &[], "items", Opts::default()) {
        Err(GcpError::Api { status: 404, .. }) => Ok(Vec::new()),
        other => other,
    }
}

pub fn security_policies(gcp: &Gcp) -> Result<Vec<SecurityPolicy>> {
    gcp.list(&format!("{}/global/securityPolicies", base(gcp)), &[], "items", Opts::default())
}

/// An aggregated list: `items` maps each scope (`global`, `regions/us-east1`…) to its own list,
/// or to a warning when it has none. A scope that could not be reached is a failed read: what
/// it holds could point at anything.
fn aggregated<T: DeserializeOwned>(gcp: &Gcp, collection: &str) -> Result<Vec<T>> {
    let url = format!("{}/aggregated/{collection}", base(gcp));
    let mut all = Vec::new();
    let mut token = String::new();
    for _ in 0..MAX_PAGES {
        let query: Vec<(&str, &str)> = if token.is_empty() { Vec::new() } else { vec![("pageToken", &token)] };
        let page: Value = gcp.get(&url, &query)?;
        if let Some(lost) = page.get("unreachables").and_then(Value::as_array).filter(|l| !l.is_empty()) {
            let lost: Vec<&str> = lost.iter().filter_map(Value::as_str).collect();
            return Err(GcpError::Network(format!("{collection}: could not reach {}", lost.join(", "))));
        }
        let scopes = page.get("items").and_then(Value::as_object).into_iter().flatten();
        for list in scopes.filter_map(|(_, scope)| scope.get(collection)) {
            let items: Vec<T> = serde_json::from_value(list.clone())
                .map_err(|e| GcpError::Network(format!("unexpected response from Google Cloud: {e}")))?;
            all.extend(items);
        }
        match page.get("nextPageToken").and_then(Value::as_str).filter(|t| !t.is_empty() && *t != token) {
            Some(next) => token = next.to_string(),
            None => break,
        }
    }
    Ok(all)
}

pub fn backend_services(gcp: &Gcp) -> Result<Vec<BackendService>> {
    aggregated(gcp, "backendServices")
}

pub fn url_maps(gcp: &Gcp) -> Result<Vec<UrlMap>> {
    aggregated(gcp, "urlMaps")
}

/// Target HTTP and HTTPS proxies.
pub fn target_proxies(gcp: &Gcp) -> Result<Vec<TargetProxy>> {
    let (https, http) = std::thread::scope(|s| {
        let http = s.spawn(|| aggregated::<TargetProxy>(gcp, "targetHttpProxies"));
        let https = aggregated::<TargetProxy>(gcp, "targetHttpsProxies");
        (https, http.join().unwrap_or_else(|_| Err(GcpError::Network("a read failed unexpectedly".into()))))
    });
    let mut all = https?;
    all.extend(http?);
    Ok(all)
}

pub fn forwarding_rules(gcp: &Gcp) -> Result<Vec<ForwardingRule>> {
    aggregated(gcp, "forwardingRules")
}

pub fn ssl_certificates(gcp: &Gcp) -> Result<Vec<SslCertificate>> {
    aggregated(gcp, "sslCertificates")
}

/// What a URL names, without its host and project, to compare two ways of writing the same
/// resource: `regions/us-east1/networkEndpointGroups/shop-neg`. A reference to a resource of
/// another project compares equal to one of the same name here, which can hide a leftover and
/// never makes one up.
pub fn reference(url: &str) -> &str {
    let tail = url.split_once("/projects/").map_or(url, |(_, t)| t);
    let tail = tail.strip_prefix("projects/").unwrap_or(tail);
    if url.contains("/projects/") || url.starts_with("projects/") {
        tail.split_once('/').map_or(tail, |(_, rest)| rest)
    } else {
        tail
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::gcp::mock::{MockServer, Reply};

    fn client(server: &MockServer) -> std::sync::Arc<Gcp> {
        Gcp::with("p", "us-east1", crate::gcp::auth::tests::fixed("t"), Some(server.url()))
    }

    #[test]
    fn a_reference_is_what_a_url_names() {
        let long = "https://www.googleapis.com/compute/v1/projects/p/regions/us-east1/networkEndpointGroups/shop-neg";
        assert_eq!(reference(long), "regions/us-east1/networkEndpointGroups/shop-neg");
        assert_eq!(reference("projects/123/global/backendServices/b"), "global/backendServices/b");
        assert_eq!(reference("//compute.googleapis.com/projects/123/global/urlMaps/m"), "global/urlMaps/m");
        assert_eq!(reference("global/backendServices/b"), "global/backendServices/b");
    }

    #[test]
    fn aggregated_lists_join_their_scopes_and_follow_their_pages() {
        let server = MockServer::start(vec![
            Reply::json(
                200,
                r#"{"items":{
                    "global":{"backendServices":[{"name":"a","loadBalancingScheme":"EXTERNAL_MANAGED","iap":{"enabled":true,"oauth2ClientId":"x"}}]},
                    "regions/us-east1":{"backendServices":[{"name":"b","region":"https://x/regions/us-east1"}]},
                    "regions/us-west1":{"warning":{"code":"NO_RESULTS_ON_PAGE"}}},
                    "nextPageToken":"t2"}"#,
            ),
            Reply::json(200, r#"{"items":{"global":{"backendServices":[{"name":"c"}]}}}"#),
        ]);
        let gcp = client(&server);
        let all = backend_services(&gcp).unwrap();
        assert_eq!(all.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(), ["a", "b", "c"]);
        assert!(all[0].iap.enabled);
        assert_eq!((all[0].region_name(), all[1].region_name()), ("", "us-east1"));
        let paths: Vec<String> = server.requests().iter().map(|r| r.path.clone()).collect();
        assert_eq!(
            paths,
            [
                "/compute/compute/v1/projects/p/aggregated/backendServices",
                "/compute/compute/v1/projects/p/aggregated/backendServices?pageToken=t2"
            ]
        );
    }

    #[test]
    fn a_scope_out_of_reach_is_a_failed_read() {
        let server = MockServer::start(vec![Reply::json(
            200,
            r#"{"items":{"global":{"urlMaps":[{"name":"a"}]}},"unreachables":["regions/us-east1"]}"#,
        )]);
        let e = url_maps(&client(&server)).unwrap_err();
        assert!(e.short().contains("regions/us-east1"), "{e}");
    }

    #[test]
    fn a_url_map_names_its_services_everywhere() {
        let map: UrlMap = serde_json::from_value(serde_json::json!({
            "name": "m",
            "defaultService": "global/backendServices/default",
            "hostRules": [{"hosts": ["shop.example.com"], "pathMatcher": "shop"}],
            "pathMatchers": [{
                "name": "shop",
                "defaultService": "global/backendServices/shop",
                "pathRules": [{"paths": ["/api/*"], "service": "global/backendServices/shop-public"}],
                "routeRules": [{"priority": 1, "routeAction": {"weightedBackendServices": [
                    {"backendService": "global/backendServices/canary", "weight": 1}],
                    "requestMirrorPolicy": {"backendService": "global/backendServices/mirror"}}}]
            }]
        }))
        .unwrap();
        let mut named = map.services();
        named.sort();
        let suffix: Vec<&str> = named.iter().map(|s| s.rsplit('/').next().unwrap()).collect();
        assert_eq!(suffix, ["canary", "default", "mirror", "shop", "shop-public"]);
        assert_eq!((map.path_rules(), map.route_rules()), (1, 1));
        assert_eq!(map.path_matchers[0].services().len(), 4);
    }

    #[test]
    fn negs_are_read_per_region_and_an_unknown_one_has_none() {
        let server = MockServer::start(vec![
            Reply::json(
                200,
                r#"{"items":[{"name":"shop-neg","region":"https://x/regions/us-east1","networkEndpointType":"SERVERLESS","cloudRun":{"service":"shop"}}]}"#,
            ),
            Reply::json(404, r#"{"error":{"code":404,"status":"NOT_FOUND","message":"no such region"}}"#),
        ]);
        let gcp = client(&server);
        let negs_read = negs(&gcp, "us-east1").unwrap();
        assert_eq!((negs_read[0].cloud_run.service.as_str(), negs_read[0].region_name()), ("shop", "us-east1"));
        assert!(negs(&gcp, "nowhere1").unwrap().is_empty());
        assert_eq!(server.requests()[0].path, "/compute/compute/v1/projects/p/regions/us-east1/networkEndpointGroups");
    }
}
