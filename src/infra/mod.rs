//! The platform: what every app of a project shares. The load balancer and its limits, the pieces
//! left behind when an app goes, which apps can be reached without going through it, certificates
//! and the image registry. All of it is derived from what points to what, never from names.
//!
//! Everything here reads. Whether an app answers at its `run.app` URL is worked out from its
//! configuration: no request ever goes to an app.
//!
//! The terminal UI, the commands and the MCP server all read through here: [`read`] reports each
//! part as it comes, [`Snapshot`] holds them, and the tables and [`findings`] are made from it.

pub mod cmd;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Mutex;

use serde_json::{Value, json};

use crate::apps::{Column, Tone};
use crate::builds::read_all;
use crate::costs::report::bytes;
use crate::findings::{self, Finding, Level};
use crate::gcp::certmanager::{self, Certificate};
use crate::gcp::compute::{
    self, BackendService, ForwardingRule, NetworkEndpointGroup, Quota, Region, SecurityPolicy, SslCertificate,
    TargetProxy, UrlMap, reference,
};
use crate::gcp::run::{self, IamPolicy, Service};
use crate::gcp::{self, Gcp, GcpError, artifacts, console_url};
use crate::util::{fmt_utc, parse_rfc3339};

/// A backend quota at this share is worth a look, and at this one needs attention.
const QUOTA_WARN: u64 = 80;
const QUOTA_BAD: u64 = 95;
/// Orphans above this share of the backend quota need attention.
const ORPHANS_BAD: u64 = 10;
/// A certificate expiring within this many days needs attention.
const CERT_DAYS: f64 = 30.0;

const INGRESS_ALL: &str = "INGRESS_TRAFFIC_ALL";
const INGRESS_LB: &str = "INGRESS_TRAFFIC_INTERNAL_LOAD_BALANCER";
const INGRESS_INTERNAL: &str = "INGRESS_TRAFFIC_INTERNAL_ONLY";

/// What the Compute Engine API says of the project's load balancers.
#[derive(Debug, Clone, Default)]
pub struct Lb {
    pub quotas: Vec<Quota>,
    /// The regions read, with their quotas.
    pub regions: Vec<Region>,
    pub backends: Vec<BackendService>,
    pub url_maps: Vec<UrlMap>,
    pub proxies: Vec<TargetProxy>,
    pub rules: Vec<ForwardingRule>,
    pub negs: Vec<NetworkEndpointGroup>,
    pub certificates: Vec<SslCertificate>,
    pub policies: Vec<SecurityPolicy>,
}

/// The Cloud Run services of the project.
#[derive(Debug, Clone, Default)]
pub struct Services {
    pub list: Vec<Service>,
    /// Regions that could not be asked: what they hold is not known either way.
    pub unreachable: Vec<String>,
}

static NO_SERVICES: Services = Services { list: Vec::new(), unreachable: Vec::new() };

/// A repository, as much as a check needs.
#[derive(Debug, Clone)]
pub struct Repo {
    pub id: String,
    pub location: String,
    pub format: String,
    pub mode: String,
    pub size: u64,
    pub cleanup: Cleanup,
}

/// What a repository's cleanup policies do.
#[derive(Debug, Clone)]
pub enum Cleanup {
    /// None at all.
    Nothing,
    /// These delete what they match.
    Deletes(Vec<String>),
    /// Only `KEEP` rules: nothing is ever deleted.
    KeepOnly(Vec<String>),
    /// Policies are only tried: nothing is deleted.
    DryRun(Vec<String>),
    /// The repository could not be read.
    Unread(GcpError),
}

/// Who may invoke a service, of what matters here.
#[derive(Debug, Clone, PartialEq)]
pub enum Invoker {
    /// Only those IAM names; who they are is not kept.
    Private,
    /// Anyone (`allUsers`), or anyone with a Google account (`allAuthenticatedUsers`).
    Public { member: String, role: String },
}

/// Who may invoke, from a service's IAM policy. A role is taken to invoke unless it plainly only
/// views: custom roles can hold `run.routes.invoke`, and a false alarm costs less than a miss.
pub fn invoker_of(policy: &IamPolicy) -> Invoker {
    let mut found: Option<Invoker> = None;
    for binding in policy.bindings.iter().filter(|b| !b.role.to_ascii_lowercase().ends_with("viewer")) {
        for member in ["allUsers", "allAuthenticatedUsers"] {
            let better = !matches!(&found, Some(Invoker::Public { member: m, .. }) if m == "allUsers");
            if binding.members.iter().any(|m| m == member) && better {
                found = Some(Invoker::Public { member: member.to_string(), role: binding.role.clone() });
            }
        }
    }
    found.unwrap_or(Invoker::Private)
}

// ---- reading ------------------------------------------------------------------------------------

/// Which parts of the platform to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Want {
    pub limits: bool,
    pub orphans: bool,
    pub exposure: bool,
    pub certificates: bool,
    pub images: bool,
}

impl Want {
    pub const ALL: Want = Want { limits: true, orphans: true, exposure: true, certificates: true, images: true };

    /// Only `section`.
    pub fn only(section: Section) -> Want {
        Want {
            limits: section == Section::Limits,
            orphans: section == Section::Orphans,
            exposure: section == Section::Exposure,
            certificates: section == Section::Certificates,
            images: section == Section::Images,
        }
    }

    fn lb(self) -> bool {
        self.limits || self.orphans || self.exposure || self.certificates
    }

    fn services(self) -> bool {
        self.orphans || self.exposure
    }
}

/// A part of the platform, as it comes.
pub enum Part {
    Services(gcp::Result<Services>),
    Invokers(HashMap<String, gcp::Result<Invoker>>),
    Lb(Box<gcp::Result<Lb>>),
    CertManager(gcp::Result<Vec<Certificate>>),
    Repos(gcp::Result<Vec<Repo>>),
}

/// Reads what `want` asks for and hands over each part as it comes, on any thread. Each read is
/// independent: one that fails leaves the others standing.
pub fn read(gcp: &Gcp, want: Want, each: impl Fn(Part) + Sync) {
    let each = &each;
    std::thread::scope(|s| {
        if want.certificates {
            s.spawn(move || each(Part::CertManager(certmanager::certificates(gcp))));
        }
        if want.images {
            s.spawn(move || each(Part::Repos(read_repos(gcp))));
        }
        let services = want.services().then(|| read_services(gcp));
        let listed = services.as_ref().and_then(|s| s.as_ref().ok()).map(|s| s.list.as_slice());
        if let Some(services) = &services {
            each(Part::Services(services.clone()));
        }
        // NEGs are read where services are, and where backends say they are.
        let regions: BTreeSet<String> = listed.into_iter().flatten().map(|s| s.region().to_string()).collect();
        std::thread::scope(|inner| {
            if want.exposure
                && let Some(list) = listed
            {
                inner.spawn(move || each(Part::Invokers(read_invokers(gcp, list))));
            }
            if want.lb() {
                each(Part::Lb(Box::new(read_lb(gcp, &regions))));
            }
        });
    });
}

/// Every service of the project, whatever the environment's region: a load balancer spans regions.
/// The wildcard says where they are, and each region is then read page by page, so a list cut short
/// can never make a service look deleted.
fn read_services(gcp: &Gcp) -> gcp::Result<Services> {
    // The environment's region is read at once: the wildcard is only needed to find the others.
    let own = (!gcp.region.is_empty()).then_some(gcp.region.as_str());
    let (every, read_own) = std::thread::scope(|s| {
        let read_own = s.spawn(|| own.map(|r| run::in_region(gcp, r)).transpose());
        (run::every_region(gcp), joined(read_own))
    });
    let every = every?;
    let others: BTreeSet<&str> = every.services.iter().map(Service::region).filter(|r| Some(*r) != own).collect();
    let others: Vec<&str> = others.into_iter().collect();
    let mut by_name: BTreeMap<String, Service> = BTreeMap::new();
    by_name.extend(read_own?.unwrap_or_default().into_iter().map(|s| (s.name.clone(), s)));
    for read in read_all(&others, |r| run::in_region(gcp, r)) {
        by_name.extend(read?.into_iter().map(|s| (s.name.clone(), s)));
    }
    // A service that appeared between the two reads.
    for s in every.services {
        by_name.entry(s.name.clone()).or_insert(s);
    }
    let mut list: Vec<Service> = by_name.into_values().collect();
    list.sort_by(|a, b| a.id().cmp(b.id()));
    Ok(Services { list, unreachable: every.unreachable })
}

fn joined<T>(read: std::thread::ScopedJoinHandle<'_, gcp::Result<T>>) -> gcp::Result<T> {
    read.join().unwrap_or_else(|_| Err(GcpError::Network("a read failed unexpectedly".into())))
}

fn read_lb(gcp: &Gcp, run_regions: &BTreeSet<String>) -> gcp::Result<Lb> {
    let (quotas, backends, url_maps, proxies, rules, certificates, policies) = std::thread::scope(|s| {
        let quotas = s.spawn(|| compute::project_quotas(gcp));
        let backends = s.spawn(|| compute::backend_services(gcp));
        let url_maps = s.spawn(|| compute::url_maps(gcp));
        let proxies = s.spawn(|| compute::target_proxies(gcp));
        let rules = s.spawn(|| compute::forwarding_rules(gcp));
        let certificates = s.spawn(|| compute::ssl_certificates(gcp));
        let policies = s.spawn(|| compute::security_policies(gcp));
        (
            joined(quotas),
            joined(backends),
            joined(url_maps),
            joined(proxies),
            joined(rules),
            joined(certificates),
            joined(policies),
        )
    });
    let backends = backends?;
    let regions: Vec<Region> = if gcp.region.is_empty() {
        compute::regions(gcp)?
    } else {
        let mut names = run_regions.clone();
        names.insert(gcp.region.clone());
        names.extend(backends.iter().flat_map(|b| &b.backends).filter_map(|g| group_region(&g.group)));
        let names: Vec<String> = names.into_iter().collect();
        read_all(&names, |name| compute::region(gcp, name)).into_iter().collect::<gcp::Result<_>>()?
    };
    let names: Vec<String> = regions.iter().map(|r| r.name.clone()).collect();
    let negs = read_all(&names, |name| compute::negs(gcp, name)).into_iter().collect::<gcp::Result<Vec<_>>>()?;
    Ok(Lb {
        quotas: quotas?,
        regions,
        backends,
        url_maps: url_maps?,
        proxies: proxies?,
        rules: rules?,
        negs: negs.into_iter().flatten().collect(),
        certificates: certificates?,
        policies: policies?,
    })
}

/// The region of a backend's NEG.
fn group_region(group: &str) -> Option<String> {
    reference(group).strip_prefix("regions/")?.split('/').next().map(str::to_string)
}

/// Who may invoke each service whose ingress lets the internet in, a call per service.
fn read_invokers(gcp: &Gcp, services: &[Service]) -> HashMap<String, gcp::Result<Invoker>> {
    let open: Vec<&Service> = services.iter().filter(|s| s.ingress == INGRESS_ALL).collect();
    let read = read_all(&open, |s| run::iam_policy(gcp, &s.name).map(|p| invoker_of(&p)));
    open.iter().map(|s| s.name.clone()).zip(read).collect()
}

/// The repositories of the client's region (of every location without one), each read to know
/// its cleanup policies, largest first.
fn read_repos(gcp: &Gcp) -> gcp::Result<Vec<Repo>> {
    let locations = if gcp.region.is_empty() { artifacts::locations(gcp)? } else { vec![gcp.region.clone()] };
    let listed = read_all(&locations, |l| artifacts::repositories(gcp, l))
        .into_iter()
        .collect::<gcp::Result<Vec<_>>>()?
        .concat();
    let read = read_all(&listed, |r| artifacts::repository(gcp, &r.name));
    let mut repos: Vec<Repo> = listed
        .iter()
        .zip(read)
        .map(|(l, r)| {
            let cleanup = match &r {
                Ok(full) => cleanup_of(full),
                Err(e) => Cleanup::Unread(e.clone()),
            };
            let source = r.as_ref().unwrap_or(l);
            Repo {
                id: l.id().to_string(),
                location: l.location().to_string(),
                format: source.format.clone(),
                mode: source.mode.clone(),
                size: source.size(),
                cleanup,
            }
        })
        .collect();
    repos.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.id.cmp(&b.id)));
    Ok(repos)
}

fn cleanup_of(repo: &artifacts::Repository) -> Cleanup {
    let ids: Vec<String> = repo.cleanup_policies.keys().cloned().collect();
    if ids.is_empty() {
        Cleanup::Nothing
    } else if repo.cleanup_policy_dry_run {
        Cleanup::DryRun(ids)
    } else if repo.cleanup_policies.values().any(|p| p.action == "DELETE") {
        Cleanup::Deletes(ids)
    } else {
        Cleanup::KeepOnly(ids)
    }
}

// ---- what was read ------------------------------------------------------------------------------

/// One of the five views of the platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Limits,
    Orphans,
    Exposure,
    Certificates,
    Images,
}

impl Section {
    pub const ALL: [Section; 5] =
        [Section::Limits, Section::Orphans, Section::Exposure, Section::Certificates, Section::Images];

    pub fn title(self) -> &'static str {
        match self {
            Section::Limits => "Limits",
            Section::Orphans => "Orphans",
            Section::Exposure => "Exposure",
            Section::Certificates => "Certificates",
            Section::Images => "Images",
        }
    }
}

/// A read that failed, and what it leaves untold.
#[derive(Debug, Clone)]
pub struct Unread {
    pub what: String,
    pub error: GcpError,
}

/// What has been read of the platform. A part is `None` until it comes, or when not asked for.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// The apps open on purpose: `platform.exposed`.
    pub exposed: Vec<String>,
    /// The environment's region, empty for every one.
    pub region: String,
    pub project: String,
    pub lb: Option<gcp::Result<Lb>>,
    pub services: Option<gcp::Result<Services>>,
    pub invokers: Option<HashMap<String, gcp::Result<Invoker>>>,
    pub cert_manager: Option<gcp::Result<Vec<Certificate>>>,
    pub repos: Option<gcp::Result<Vec<Repo>>>,
}

impl Snapshot {
    pub fn new(gcp: &Gcp, exposed: &[String]) -> Snapshot {
        Snapshot {
            exposed: exposed.to_vec(),
            region: gcp.region.clone(),
            project: gcp.project.clone(),
            ..Snapshot::default()
        }
    }

    /// Reads what `want` asks for and waits for all of it.
    pub fn load(gcp: &Gcp, exposed: &[String], want: Want) -> Snapshot {
        let snapshot = Mutex::new(Snapshot::new(gcp, exposed));
        read(gcp, want, |part| snapshot.lock().unwrap_or_else(|e| e.into_inner()).add(part));
        snapshot.into_inner().unwrap_or_else(|e| e.into_inner())
    }

    pub fn add(&mut self, part: Part) {
        match part {
            Part::Services(list) => self.services = Some(list),
            Part::Invokers(map) => self.invokers = Some(map),
            Part::Lb(lb) => self.lb = Some(*lb),
            Part::CertManager(list) => self.cert_manager = Some(list),
            Part::Repos(list) => self.repos = Some(list),
        }
    }

    fn lb(&self) -> Option<&Lb> {
        self.lb.as_ref()?.as_ref().ok()
    }

    /// The services, when known: an API that is off holds none.
    fn services(&self) -> Option<&Services> {
        match self.services.as_ref()? {
            Ok(services) => Some(services),
            Err(e) if e.is_api_off() => Some(&NO_SERVICES),
            Err(_) => None,
        }
    }

    /// Reads that failed, an API that is off not among them: it holds nothing to tell.
    pub fn unread(&self) -> Vec<Unread> {
        let mut out = Vec::new();
        let mut add = |what: &str, e: &GcpError| {
            if !e.is_api_off() {
                out.push(Unread { what: what.to_string(), error: e.clone() });
            }
        };
        if let Some(Err(e)) = &self.lb {
            add("the load balancer (limits, orphans, exposure)", e);
        }
        if let Some(Err(e)) = &self.services {
            add("the Cloud Run services (exposure, orphans)", e);
        }
        if let Some(Err(e)) = &self.cert_manager {
            add("Certificate Manager's certificates", e);
        }
        if let Some(Err(e)) = &self.repos {
            add("the image repositories", e);
        }
        if let Some(map) = &self.invokers {
            let mut failed: Vec<(&String, &GcpError)> =
                map.iter().filter_map(|(k, v)| v.as_ref().err().map(|e| (k, e))).collect();
            failed.sort_by_key(|(k, _)| *k);
            if let Some((_, e)) = failed.first() {
                let names: Vec<&str> = failed.iter().map(|(k, _)| run::last_segment(k)).collect();
                add(&format!("who may invoke {}", names.join(", ")), e);
            }
        }
        if let Some(Ok(repos)) = &self.repos {
            let failed: Vec<(&str, &GcpError)> = repos
                .iter()
                .filter_map(|r| match &r.cleanup {
                    Cleanup::Unread(e) => Some((r.id.as_str(), e)),
                    _ => None,
                })
                .collect();
            if let Some((_, e)) = failed.first() {
                let names: Vec<&str> = failed.iter().map(|(n, _)| *n).collect();
                add(&format!("the cleanup policies of {}", names.join(", ")), e);
            }
        }
        out
    }

    /// What the reader should know besides the numbers.
    pub fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        let mut off = |what: &str, part: Option<&GcpError>| {
            if part.is_some_and(GcpError::is_api_off) {
                notes.push(format!("The {what} API is off in this project: there is nothing to read in it."));
            }
        };
        off("Compute Engine", self.lb.as_ref().and_then(|r| r.as_ref().err()));
        off("Cloud Run", self.services.as_ref().and_then(|r| r.as_ref().err()));
        off("Certificate Manager", self.cert_manager.as_ref().and_then(|r| r.as_ref().err()));
        off("Artifact Registry", self.repos.as_ref().and_then(|r| r.as_ref().err()));
        if let Some(lb) = self.lb() {
            let masks: Vec<&str> =
                lb.negs.iter().filter(|n| !n.cloud_run.url_mask.is_empty()).map(|n| n.name.as_str()).collect();
            if !masks.is_empty() {
                notes.push(format!(
                    "URL masks reach whichever service a request names, so which services these NEGs reach cannot be told: {}.",
                    masks.join(", ")
                ));
            }
        }
        notes
    }

    /// Still waiting for what `section` is made of.
    pub fn reading(&self, section: Section) -> bool {
        match section {
            Section::Limits => self.lb.is_none(),
            Section::Orphans => self.lb.is_none() || self.services.is_none(),
            Section::Exposure => {
                self.lb.is_none()
                    || self.services.is_none()
                    || (self.invokers.is_none() && matches!(self.services, Some(Ok(_))))
            }
            Section::Certificates => self.lb.is_none() || self.cert_manager.is_none(),
            Section::Images => self.repos.is_none(),
        }
    }

    /// Why `section` has nothing to show, when a read failed.
    pub fn failure(&self, section: Section) -> Option<&GcpError> {
        let lb = self.lb.as_ref().and_then(|r| r.as_ref().err());
        let services = self.services.as_ref().and_then(|r| r.as_ref().err());
        match section {
            Section::Limits => lb,
            Section::Orphans => lb.or(services),
            Section::Exposure => services,
            Section::Certificates => self.cert_manager.as_ref().and_then(|r| r.as_ref().err()).or(lb),
            Section::Images => self.repos.as_ref().and_then(|r| r.as_ref().err()),
        }
    }
}

// ---- limits -------------------------------------------------------------------------------------

/// Something counted against a limit.
#[derive(Debug, Clone, PartialEq)]
pub struct Limit {
    pub what: String,
    /// The quota metric, when there is one.
    pub metric: String,
    pub used: u64,
    /// `None`: no API gives one.
    pub limit: Option<u64>,
    /// What the metric says is in use, when it differs from what `used` was taken from.
    pub reported: Option<u64>,
    pub listed: Option<u64>,
    /// One of the quotas that hold backend services.
    pub backends: bool,
}

impl Limit {
    /// The share in use, rounded down.
    pub fn percent(&self) -> Option<u64> {
        let limit = self.limit?;
        // A limit of zero is full as soon as anything counts against it.
        Some(self.used.saturating_mul(100).checked_div(limit).unwrap_or(if self.used == 0 { 0 } else { 100 }))
    }

    fn tone(&self) -> Tone {
        match self.limit {
            Some(l) if self.used.saturating_mul(100) >= l.saturating_mul(QUOTA_BAD) && self.used > 0 => Tone::Bad,
            Some(l) if self.used.saturating_mul(100) >= l.saturating_mul(QUOTA_WARN) && self.used > 0 => Tone::Warn,
            _ => Tone::Plain,
        }
    }

    /// How `used` was arrived at, when the metric alone does not say it.
    pub fn note(&self) -> String {
        if self.limit.is_none() {
            return "no limit in the API".into();
        }
        match (self.reported, self.listed) {
            (Some(r), Some(l)) if l > r => format!("{l} counted from the list: the metric reports {r}"),
            (Some(r), Some(l)) if r > l => format!("{r} by the metric: {l} found in the regions read"),
            _ => String::new(),
        }
    }

    /// The figure compared to the limit comes from the list, not from the metric.
    pub fn counted(&self) -> bool {
        matches!((self.reported, self.listed), (Some(r), Some(l)) if l > r)
    }

    fn json(&self) -> Value {
        json!({
            "what": self.what,
            "metric": Some(&self.metric).filter(|m| !m.is_empty()),
            "used": self.used,
            "limit": self.limit,
            "percent": self.percent(),
            "metricReports": self.reported,
            "listed": self.listed,
            "countedFromList": self.counted(),
            "note": Some(self.note()).filter(|n| !n.is_empty()),
        })
    }
}

/// The quota metric that counts a backend service, by scope, scheme and protocol. There is one per
/// kind of load balancer: `BACKEND_SERVICES` is only the classic external one's, and reads 0 on a
/// project whose services all use the global external one.
pub fn backend_metric(b: &BackendService) -> &'static str {
    let scheme = b.load_balancing_scheme.as_str();
    if !b.region_name().is_empty() {
        return match scheme {
            "EXTERNAL_MANAGED" => "REGIONAL_EXTERNAL_MANAGED_BACKEND_SERVICES",
            "EXTERNAL" => "REGIONAL_EXTERNAL_NETWORK_LB_BACKEND_SERVICES",
            "INTERNAL_MANAGED" => "REGIONAL_INTERNAL_MANAGED_BACKEND_SERVICES",
            "INTERNAL_SELF_MANAGED" => "REGIONAL_INTERNAL_TRAFFIC_DIRECTOR_BACKEND_SERVICES",
            _ => "REGIONAL_INTERNAL_LB_BACKEND_SERVICES",
        };
    }
    let proxied = matches!(b.protocol.as_str(), "TCP" | "SSL");
    match scheme {
        "EXTERNAL_MANAGED" | "EXTERNAL" if proxied => "GLOBAL_EXTERNAL_PROXY_LB_BACKEND_SERVICES",
        "EXTERNAL_MANAGED" => "GLOBAL_EXTERNAL_MANAGED_BACKEND_SERVICES",
        "INTERNAL_MANAGED" => "GLOBAL_INTERNAL_MANAGED_BACKEND_SERVICES",
        "INTERNAL_SELF_MANAGED" => "GLOBAL_INTERNAL_TRAFFIC_DIRECTOR_BACKEND_SERVICES",
        _ => "BACKEND_SERVICES",
    }
}

fn quota<'a>(quotas: &'a [Quota], metric: &str) -> Option<&'a Quota> {
    quotas.iter().find(|q| q.metric == metric)
}

/// The fallback when a project's quotas do not hold the metric a backend service is counted in.
const ANY_BACKEND: &str = "BACKEND_SERVICES";

impl Lb {
    fn region_quotas(&self, region: &str) -> &[Quota] {
        self.regions.iter().find(|r| r.name == region).map_or(&[], |r| &r.quotas)
    }

    /// Everything counted against a limit: the backend quotas first, the fullest on top.
    pub fn limits(&self) -> Vec<Limit> {
        let mut rows = self.backend_limits();
        rows.sort_by(|a, b| b.percent().cmp(&a.percent()).then_with(|| a.what.cmp(&b.what)));
        rows.extend(self.neg_limits());
        for (what, metric, listed) in [
            ("URL maps", "URL_MAPS", Some(self.global_url_maps())),
            ("Target HTTPS proxies", "TARGET_HTTPS_PROXIES", None),
            ("Forwarding rules", "FORWARDING_RULES", None),
            ("Backend buckets", "BACKEND_BUCKETS", None),
            ("SSL certificates", "SSL_CERTIFICATES", None),
            ("Security policies", "SECURITY_POLICIES", Some(self.policies.len() as u64)),
        ] {
            if let Some(q) = quota(&self.quotas, metric) {
                rows.push(measured(what, metric, q, listed, false));
            }
        }
        // No quota holds these: what is in a URL map is shown without a limit.
        for map in &self.url_maps {
            let rule = |what: &str, used: usize| Limit {
                what: format!("{what} in {}", map.name),
                metric: String::new(),
                used: used as u64,
                limit: None,
                reported: None,
                listed: None,
                backends: false,
            };
            rows.push(rule("Host rules", map.host_rules.len()));
            rows.push(rule("Path matchers", map.path_matchers.len()));
            rows.push(rule("Path rules", map.path_rules()));
            if map.route_rules() > 0 {
                rows.push(rule("Route rules", map.route_rules()));
            }
        }
        rows
    }

    fn global_url_maps(&self) -> u64 {
        self.url_maps.iter().filter(|m| !reference(&m.self_link).starts_with("regions/")).count() as u64
    }

    fn backend_limits(&self) -> Vec<Limit> {
        // (region, metric) → how many were listed; empty region: the project.
        let mut listed: BTreeMap<(String, String), u64> = BTreeMap::new();
        for b in &self.backends {
            *listed.entry((b.region_name().to_string(), backend_metric(b).to_string())).or_default() += 1;
        }
        // A metric that reports some the list does not hold is shown too.
        let reporting = |region: &str, quotas: &[Quota], listed: &mut BTreeMap<(String, String), u64>| {
            for q in quotas.iter().filter(|q| q.metric.ends_with(ANY_BACKEND) && q.usage > 0.0) {
                listed.entry((region.to_string(), q.metric.clone())).or_default();
            }
        };
        reporting("", &self.quotas, &mut listed);
        for r in &self.regions {
            reporting(&r.name, &r.quotas, &mut listed);
        }
        if listed.is_empty() {
            listed.insert((String::new(), ANY_BACKEND.to_string()), 0);
        }
        listed
            .into_iter()
            .map(|((region, metric), count)| {
                let quotas = if region.is_empty() { self.quotas.as_slice() } else { self.region_quotas(&region) };
                let what = if region.is_empty() {
                    "Backend services".to_string()
                } else {
                    format!("Backend services in {region}")
                };
                match quota(quotas, &metric) {
                    Some(q) => measured(&what, &metric, q, Some(count), true),
                    // Not a metric this project has: the broadest limit, against the count.
                    None => Limit {
                        what,
                        metric: metric.clone(),
                        used: count,
                        limit: quota(&self.quotas, ANY_BACKEND).map(|q| q.limit as u64),
                        reported: Some(0),
                        listed: Some(count),
                        backends: true,
                    },
                }
            })
            .collect()
    }

    fn neg_limits(&self) -> Vec<Limit> {
        const METRIC: &str = "NETWORK_ENDPOINT_GROUPS";
        let mut rows = Vec::new();
        if let Some(q) = quota(&self.quotas, METRIC) {
            rows.push(measured("NEGs", METRIC, q, Some(self.negs.len() as u64), false));
        }
        for r in &self.regions {
            let count = self.negs.iter().filter(|n| n.region_name() == r.name).count() as u64;
            if let Some(q) = quota(&r.quotas, METRIC).filter(|q| count > 0 || q.usage > 0.0) {
                rows.push(measured(&format!("NEGs in {}", r.name), METRIC, q, Some(count), false));
            }
        }
        rows
    }

    /// Whether a NEG has any backend, and which backends name it: by reference.
    fn backends_of_neg(&self, neg: &NetworkEndpointGroup) -> Vec<&BackendService> {
        let own = reference(&neg.self_link);
        self.backends.iter().filter(|b| b.backends.iter().any(|g| reference(&g.group) == own)).collect()
    }

    /// Backend services named by some URL map.
    fn mapped_backends(&self) -> HashSet<String> {
        self.url_maps.iter().flat_map(UrlMap::services).map(|s| reference(&s).to_string()).collect()
    }

    /// URL maps a forwarding rule reaches, through its proxy.
    fn served_url_maps(&self) -> HashSet<&str> {
        let targets: HashSet<&str> = self.rules.iter().map(|r| reference(&r.target)).collect();
        self.proxies
            .iter()
            .filter(|p| targets.contains(reference(&p.self_link)))
            .map(|p| reference(&p.url_map))
            .collect()
    }
}

/// `used` is the higher of what the metric says and what was listed: a metric can lag, or not count.
fn measured(what: &str, metric: &str, q: &Quota, listed: Option<u64>, backends: bool) -> Limit {
    let reported = q.usage as u64;
    Limit {
        what: what.to_string(),
        metric: metric.to_string(),
        used: reported.max(listed.unwrap_or(0)),
        limit: Some(q.limit as u64),
        reported: Some(reported),
        listed,
        backends,
    }
}

// ---- orphans ------------------------------------------------------------------------------------

/// A piece nothing leads to, or that leads nowhere.
#[derive(Debug, Clone, PartialEq)]
pub struct Orphan {
    /// `backend service`, `NEG` or `host rule`.
    pub kind: &'static str,
    pub name: String,
    /// A region or a URL map.
    pub place: String,
    /// All that is wrong with it.
    pub why: Vec<String>,
}

impl Orphan {
    fn json(&self) -> Value {
        json!({ "kind": self.kind, "name": self.name, "where": self.place, "why": self.why })
    }
}

/// The leftovers of `lb`. `services` is `None` when they are not known: what depends on them is
/// then not said, so a failed read never reads as a deleted app.
pub fn orphans(lb: &Lb, services: Option<&Services>) -> Vec<Orphan> {
    let mut out = Vec::new();
    let mapped = lb.mapped_backends();
    let direct: HashSet<&str> =
        lb.rules.iter().filter(|r| !r.backend_service.is_empty()).map(|r| reference(&r.backend_service)).collect();
    for b in &lb.backends {
        let own = reference(&b.self_link);
        let mut why = Vec::new();
        // A TCP or SSL proxy or a passthrough rule uses it without a URL map.
        if !mapped.contains(own) && !direct.contains(own) && b.used_by.is_empty() {
            why.push("no URL map uses it".to_string());
        }
        if b.backends.is_empty() {
            why.push("it has no backends".to_string());
        }
        if !why.is_empty() {
            let place = if b.region_name().is_empty() { "global" } else { b.region_name() };
            out.push(Orphan { kind: "backend service", name: b.name.clone(), place: place.to_string(), why });
        }
    }
    let exists = |region: &str, service: &str| {
        services.is_none_or(|known| {
            known.unreachable.iter().any(|r| r == region)
                || known.list.iter().any(|s| s.id() == service && s.region() == region)
        })
    };
    for n in &lb.negs {
        let mut why = Vec::new();
        if lb.backends_of_neg(n).is_empty() {
            why.push("no backend uses it".to_string());
        }
        if !n.cloud_run.service.is_empty() && !exists(n.region_name(), &n.cloud_run.service) {
            why.push(format!("its Cloud Run service {} does not exist", n.cloud_run.service));
        }
        if !why.is_empty() {
            out.push(Orphan { kind: "NEG", name: n.name.clone(), place: n.region_name().to_string(), why });
        }
    }
    let mut hosts = Vec::new();
    for map in &lb.url_maps {
        for rule in &map.host_rules {
            let Some(matcher) = map.path_matchers.iter().find(|p| p.name == rule.path_matcher) else { continue };
            let Some(behind) = services_behind(lb, &matcher.services()) else { continue };
            if !behind.is_empty() && behind.iter().all(|(region, s)| !exists(region, s)) {
                let names: BTreeSet<&str> = behind.iter().map(|(_, s)| s.as_str()).collect();
                let names: Vec<&str> = names.into_iter().collect();
                hosts.push(Orphan {
                    kind: "host rule",
                    name: rule.hosts.join(", "),
                    place: map.name.clone(),
                    why: vec![format!("its Cloud Run service {} does not exist", names.join(", "))],
                });
            }
        }
    }
    hosts.sort_by(|a, b| a.name.cmp(&b.name));
    out.extend(hosts);
    out
}

/// The Cloud Run services `targets` lead to, as (region, id); `None` when one of them leads to
/// something else (a backend bucket, a VM, a URL mask), which says nothing about a host being dead.
fn services_behind(lb: &Lb, targets: &[String]) -> Option<Vec<(String, String)>> {
    let mut found = Vec::new();
    for target in targets {
        let backend = lb.backends.iter().find(|b| reference(&b.self_link) == reference(target))?;
        if backend.backends.is_empty() {
            return None;
        }
        for group in &backend.backends {
            let neg = lb.negs.iter().find(|n| reference(&n.self_link) == reference(&group.group))?;
            if neg.cloud_run.service.is_empty() {
                return None;
            }
            found.push((neg.region_name().to_string(), neg.cloud_run.service.clone()));
        }
    }
    Some(found)
}

// ---- exposure -----------------------------------------------------------------------------------

/// A backend through which a load balancer reaches a service.
#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    pub backend: String,
    pub iap: bool,
    /// A Cloud Armor policy.
    pub armor: Option<String>,
}

/// Whether a service can be called from the internet at its `run.app` URL.
#[derive(Debug, Clone, PartialEq)]
pub enum Reach {
    No,
    /// With what makes it so.
    Yes(String),
    /// Its invoker policy is unread.
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Exposure {
    pub service: String,
    pub region: String,
    pub url: String,
    /// `all`, `internal-and-cloud-load-balancing`, `internal`.
    pub ingress: String,
    /// Empty: no load balancer reaches it.
    pub routes: Vec<Route>,
    /// False: the load balancer's pieces are unread, so `routes` says nothing.
    pub routes_known: bool,
    /// `None`: not asked (the ingress already keeps the internet out), or not read.
    pub invoker: Option<Invoker>,
    pub reach: Reach,
    /// Why the `run.app` URL is closed, when it is.
    pub closed_by: Option<String>,
    /// Named in `platform.exposed`.
    pub listed: bool,
    console: String,
}

/// `all`, `internal-and-cloud-load-balancing`, `internal`: as `gcloud` calls them.
pub fn ingress_name(service: &Service) -> String {
    match service.ingress.as_str() {
        INGRESS_ALL => "all".to_string(),
        INGRESS_LB => "internal-and-cloud-load-balancing".to_string(),
        INGRESS_INTERNAL => "internal".to_string(),
        "" => "unknown".to_string(),
        _ => service.ingress_label(),
    }
}

impl Exposure {
    fn build(
        s: &Service,
        project: &str,
        routes: Option<&[Route]>,
        invoker: Option<&gcp::Result<Invoker>>,
        listed: bool,
    ) -> Exposure {
        let open = s.ingress == INGRESS_ALL;
        let invoker_read = invoker.and_then(|r| r.as_ref().ok()).cloned();
        let (reach, closed_by) = if !open {
            (Reach::No, Some(format!("ingress {}", ingress_name(s))))
        } else if s.default_uri_disabled {
            (Reach::No, Some("its run.app URL is turned off".to_string()))
        } else if s.iap_enabled {
            (Reach::No, Some("IAP is on for the service itself".to_string()))
        } else if s.invoker_iam_disabled {
            (Reach::Yes("ingress all and the invoker check is off (invokerIamDisabled)".to_string()), None)
        } else {
            match &invoker_read {
                Some(Invoker::Public { member, role }) => {
                    (Reach::Yes(format!("ingress all and {member} holds {role}")), None)
                }
                Some(Invoker::Private) => (Reach::No, Some("only IAM members may invoke it".to_string())),
                None => (Reach::Unknown, None),
            }
        };
        Exposure {
            service: s.id().to_string(),
            region: s.region().to_string(),
            url: s.uri.clone(),
            ingress: ingress_name(s),
            routes: routes.map(<[Route]>::to_vec).unwrap_or_default(),
            routes_known: routes.is_some(),
            invoker: invoker_read,
            reach,
            closed_by,
            listed,
            console: s.console_url(project),
        }
    }

    /// `iap`, `public`, `iap + public` (public paths split off), or `none` without a load balancer.
    pub fn gate(&self) -> &'static str {
        if !self.routes_known {
            return "?";
        }
        let iap = self.routes.iter().any(|r| r.iap);
        let public = self.routes.iter().any(|r| !r.iap);
        match (iap, public) {
            (true, true) => "iap + public",
            (true, false) => "iap",
            (false, true) => "public",
            (false, false) => "none",
        }
    }

    fn invoker_text(&self) -> String {
        match &self.invoker {
            Some(Invoker::Public { member, .. }) => member.clone(),
            Some(Invoker::Private) => "IAM".to_string(),
            None if matches!(self.reach, Reach::Unknown) => "?".to_string(),
            None => "—".to_string(),
        }
    }

    fn rank(&self) -> u8 {
        match (&self.reach, self.listed) {
            (Reach::Yes(_), false) => 0,
            (Reach::Unknown, _) => 1,
            (Reach::No, true) => 2,
            (Reach::Yes(_), true) => 3,
            (Reach::No, false) if self.ingress == "all" => 4,
            (Reach::No, false) => 5,
        }
    }

    fn note(&self, env_region: &str) -> (String, Tone) {
        let mut parts = Vec::new();
        let mut tone = Tone::Muted;
        match &self.reach {
            Reach::Yes(why) if self.listed => {
                parts.push(format!("listed: {why}"));
                tone = Tone::Warn;
            }
            Reach::Yes(why) => {
                parts.push(why.clone());
                tone = Tone::Bad;
            }
            Reach::No if self.listed => {
                parts.push(format!("listed, but no longer exposed: {}", self.closed_by.as_deref().unwrap_or("closed")));
                tone = Tone::Warn;
            }
            Reach::Unknown => {
                parts.push("who may invoke it is unread".to_string());
                tone = Tone::Warn;
            }
            Reach::No => {}
        }
        let (iap, public): (Vec<&Route>, Vec<&Route>) = self.routes.iter().partition(|r| r.iap);
        if !iap.is_empty() && !public.is_empty() {
            let names = |routes: &[&Route]| routes.iter().map(|r| r.backend.as_str()).collect::<Vec<_>>().join(", ");
            parts.push(format!("public paths split off: {} without IAP", names(&public)));
        }
        if self.ingress == "internal" && !self.routes.is_empty() {
            parts.push("ingress internal refuses an external load balancer".to_string());
        }
        if !env_region.is_empty() && self.region != env_region {
            parts.push(format!("in {}", self.region));
        }
        (parts.join(" · "), tone)
    }

    fn json(&self) -> Value {
        let (direct, why) = match &self.reach {
            Reach::No => (json!(false), None),
            Reach::Yes(why) => (json!(true), Some(why.clone())),
            Reach::Unknown => (Value::Null, None),
        };
        json!({
            "service": self.service,
            "region": self.region,
            "url": Some(&self.url).filter(|u| !u.is_empty()),
            "ingress": self.ingress,
            "loadBalancer": if self.routes_known { json!(!self.routes.is_empty()) } else { Value::Null },
            "iap": self.gate(),
            "backends": self.routes.iter().map(|r| json!({ "name": r.backend, "iap": r.iap, "armor": r.armor })).collect::<Vec<_>>(),
            "publicInvoker": match &self.invoker {
                Some(Invoker::Public { member, role }) => json!({ "member": member, "role": role }),
                _ => Value::Null,
            },
            "direct": direct,
            "why": why,
            "closedBy": self.closed_by,
            "listed": self.listed,
        })
    }
}

/// Every service with how it can be reached: what needs a look first.
pub fn exposure(snapshot: &Snapshot) -> Vec<Exposure> {
    let Some(services) = snapshot.services() else { return Vec::new() };
    let routes = snapshot.lb().map(routes_by_service);
    let mut rows: Vec<Exposure> = services
        .list
        .iter()
        .map(|s| {
            let own = routes
                .as_ref()
                .map(|r| r.get(&(s.region().to_string(), s.id().to_string())).map_or(&[][..], Vec::as_slice));
            let invoker = snapshot.invokers.as_ref().and_then(|m| m.get(&s.name));
            Exposure::build(s, &snapshot.project, own, invoker, snapshot.exposed.iter().any(|e| e == s.id()))
        })
        .collect();
    rows.sort_by(|a, b| a.rank().cmp(&b.rank()).then_with(|| a.service.cmp(&b.service)));
    rows
}

/// The live routes to each Cloud Run service: forwarding rule → proxy → URL map → backend → NEG.
fn routes_by_service(lb: &Lb) -> HashMap<(String, String), Vec<Route>> {
    let served = lb.served_url_maps();
    let live: HashSet<String> = lb
        .url_maps
        .iter()
        .filter(|m| served.contains(reference(&m.self_link)))
        .flat_map(UrlMap::services)
        .map(|s| reference(&s).to_string())
        .collect();
    let mut out: HashMap<(String, String), Vec<Route>> = HashMap::new();
    for n in lb.negs.iter().filter(|n| !n.cloud_run.service.is_empty()) {
        let key = (n.region_name().to_string(), n.cloud_run.service.clone());
        for b in lb.backends_of_neg(n) {
            if live.contains(reference(&b.self_link)) {
                let armor = Some(run::last_segment(&b.security_policy).to_string()).filter(|a| !a.is_empty());
                let route = Route { backend: b.name.clone(), iap: b.iap.enabled, armor };
                let routes = out.entry(key.clone()).or_default();
                if !routes.contains(&route) {
                    routes.push(route);
                }
            }
        }
    }
    out
}

// ---- certificates -------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Cert {
    pub name: String,
    /// `certificate manager` or `compute`: the API that holds it.
    pub source: &'static str,
    pub managed: bool,
    /// `ACTIVE`… for a managed one; empty for a self-managed one, which has no status.
    pub status: String,
    pub expires: Option<f64>,
    pub domains: Vec<String>,
    pub in_use: bool,
    /// Domains that hold it back.
    pub problems: Vec<String>,
}

impl Cert {
    fn days_left(&self, now: f64) -> Option<f64> {
        self.expires.map(|t| (t - now) / 86_400.0)
    }

    fn expiring(&self, now: f64) -> bool {
        self.days_left(now).is_some_and(|d| d <= CERT_DAYS)
    }

    fn not_active(&self) -> bool {
        self.managed && self.status != "ACTIVE"
    }

    fn expiry_text(&self, now: f64) -> String {
        match (self.expires, self.days_left(now)) {
            (Some(t), Some(d)) if d < 0.0 => format!("expired {:.0} days ago ({})", -d, fmt_utc(t)),
            (Some(t), Some(d)) => format!("expires in {d:.0} days ({})", fmt_utc(t)),
            _ => String::new(),
        }
    }

    fn json(&self, now: f64) -> Value {
        json!({
            "name": self.name,
            "source": self.source,
            "managed": self.managed,
            "status": Some(&self.status).filter(|s| !s.is_empty()),
            "expires": self.expires.map(crate::gcp::monitoring::rfc3339),
            "daysLeft": self.days_left(now).map(|d| d.floor()),
            "domains": self.domains,
            "inUse": self.in_use,
            "problems": self.problems,
        })
    }
}

/// Certificates of both APIs, the ones that need attention first.
pub fn certificates(snapshot: &Snapshot, now: f64) -> Vec<Cert> {
    let mut all = Vec::new();
    if let Some(Ok(list)) = &snapshot.cert_manager {
        for c in list {
            let managed = c.managed.as_ref();
            let domains = if c.san_dnsnames.is_empty() {
                managed.map(|m| m.domains.clone()).unwrap_or_default()
            } else {
                c.san_dnsnames.clone()
            };
            let problems = managed
                .map(|m| {
                    m.authorization_attempt_info
                        .iter()
                        .filter(|a| a.state != "AUTHORIZED")
                        .map(|a| {
                            let why = if a.failure_reason.is_empty() { &a.state } else { &a.failure_reason };
                            format!("{}: {why}", a.domain)
                        })
                        .collect()
                })
                .unwrap_or_default();
            all.push(Cert {
                name: c.id().to_string(),
                source: "certificate manager",
                managed: managed.is_some(),
                status: managed.map(|m| m.state.clone()).unwrap_or_default(),
                expires: parse_rfc3339(&c.expire_time),
                domains,
                in_use: !c.used_by.is_empty(),
                problems,
            });
        }
    }
    if let Some(lb) = snapshot.lb() {
        let named: HashSet<&str> = lb.proxies.iter().flat_map(|p| &p.ssl_certificates).map(|c| reference(c)).collect();
        for c in &lb.certificates {
            let managed = c.kind == "MANAGED";
            all.push(Cert {
                name: c.name.clone(),
                source: "compute",
                managed,
                status: if managed { c.managed.status.clone() } else { String::new() },
                expires: parse_rfc3339(&c.expire_time),
                domains: if c.managed.domains.is_empty() {
                    c.subject_alternative_names.clone()
                } else {
                    c.managed.domains.clone()
                },
                in_use: named.contains(reference(&c.self_link)),
                problems: c
                    .managed
                    .domain_status
                    .iter()
                    .filter(|(_, state)| *state != "ACTIVE")
                    .map(|(domain, state)| format!("{domain}: {state}"))
                    .collect(),
            });
        }
    }
    let urgent = |c: &Cert| !(c.not_active() || c.expiring(now));
    all.sort_by(|a, b| urgent(a).cmp(&urgent(b)).then_with(|| a.name.cmp(&b.name)));
    all
}

// ---- images -------------------------------------------------------------------------------------

impl Repo {
    fn policy_text(&self) -> String {
        match &self.cleanup {
            Cleanup::Nothing => "none".to_string(),
            Cleanup::Deletes(ids) => ids.join(", "),
            Cleanup::KeepOnly(ids) => format!("keep rules only: {}", ids.join(", ")),
            Cleanup::DryRun(ids) => format!("dry run: {}", ids.join(", ")),
            Cleanup::Unread(_) => "?".to_string(),
        }
    }

    /// What is wrong with its cleanup: a virtual repository holds nothing to clean.
    fn cleanup_problem(&self) -> Option<String> {
        if self.mode == "VIRTUAL_REPOSITORY" {
            return None;
        }
        match &self.cleanup {
            Cleanup::Nothing => Some(format!("no cleanup policy, and it holds {}", bytes(self.size))),
            Cleanup::KeepOnly(_) => {
                Some(format!("only keep rules, so nothing is deleted, and it holds {}", bytes(self.size)))
            }
            Cleanup::DryRun(_) => {
                Some(format!("cleanup policies in dry run delete nothing, and it holds {}", bytes(self.size)))
            }
            Cleanup::Deletes(_) | Cleanup::Unread(_) => None,
        }
    }

    fn json(&self) -> Value {
        json!({
            "name": self.id,
            "location": self.location,
            "format": self.format,
            "sizeBytes": self.size,
            "cleanupPolicies": match &self.cleanup {
                Cleanup::Deletes(ids) | Cleanup::KeepOnly(ids) | Cleanup::DryRun(ids) => json!(ids),
                Cleanup::Nothing => json!([]),
                Cleanup::Unread(_) => Value::Null,
            },
            "dryRun": matches!(self.cleanup, Cleanup::DryRun(_)),
            "problem": self.cleanup_problem(),
        })
    }
}

// ---- findings -----------------------------------------------------------------------------------

/// What needs a look in one section.
pub fn section_findings(snapshot: &Snapshot, section: Section, now: f64) -> Vec<Finding> {
    let mut found = match section {
        Section::Limits => snapshot.lb().map(limit_findings).unwrap_or_default(),
        Section::Orphans => snapshot.lb().map(|lb| orphan_findings(lb, snapshot.services())).unwrap_or_default(),
        Section::Exposure => exposure_findings(snapshot),
        Section::Certificates => cert_findings(snapshot, now),
        Section::Images => image_findings(snapshot),
    };
    findings::sort(&mut found);
    found
}

/// Everything that needs a look, the worst first.
pub fn findings(snapshot: &Snapshot, now: f64) -> Vec<Finding> {
    let mut found: Vec<Finding> = Section::ALL.iter().flat_map(|s| section_findings(snapshot, *s, now)).collect();
    findings::sort(&mut found);
    found
}

fn limit_findings(lb: &Lb) -> Vec<Finding> {
    let mut found = Vec::new();
    for row in lb.backend_limits() {
        let Some(limit) = row.limit else { continue };
        let what = format!(
            "{} of {limit} in use ({}%, {}){}",
            row.used,
            row.percent().unwrap_or(0),
            row.metric,
            if row.counted() { ", counted from the list: the metric does not report them" } else { "" }
        );
        let subject = row.what.to_lowercase();
        let hint = "New deploys fail at the limit, and leftovers count too: see the orphans.";
        match row.tone() {
            Tone::Bad => found.push(Finding::bad(subject, what).hint(hint)),
            Tone::Warn => found.push(Finding::warn(subject, what).hint(hint)),
            _ => {}
        }
    }
    found
}

/// The backend quota orphans are measured against: the fullest one's limit.
fn backend_quota(lb: &Lb) -> Option<u64> {
    lb.backend_limits().into_iter().filter(|r| r.limit.is_some()).max_by_key(|r| r.percent()).and_then(|r| r.limit)
}

/// How many names a finding spells out; every one is in the section itself.
const NAMES: usize = 10;

fn orphan_findings(lb: &Lb, services: Option<&Services>) -> Vec<Finding> {
    let list = orphans(lb, services);
    if list.is_empty() {
        return Vec::new();
    }
    let names: Vec<String> = list.iter().take(NAMES).map(|o| format!("{} {}", o.kind, o.name)).collect();
    let more = if list.len() > NAMES { format!(" and {} more", list.len() - NAMES) } else { String::new() };
    let what = format!("{} left behind: {}{more}", list.len(), names.join(", "));
    let hint = "Nothing here removes them: `platform --orphans` says what is wrong with each.";
    let too_many = backend_quota(lb).is_some_and(|l| (list.len() as u64) * 100 > l * ORPHANS_BAD);
    let finding = if too_many {
        let quota = backend_quota(lb).unwrap_or(0);
        Finding::bad("orphans", format!("{what}: more than {ORPHANS_BAD}% of the backend quota of {quota}"))
    } else {
        Finding::warn("orphans", what)
    };
    vec![finding.hint(hint)]
}

fn exposure_findings(snapshot: &Snapshot) -> Vec<Finding> {
    let rows = exposure(snapshot);
    let mut found = Vec::new();
    for row in &rows {
        match (&row.reach, row.listed) {
            (Reach::Yes(why), false) => {
                let behind = if row.routes.iter().any(|r| r.iap) {
                    " IAP on its load balancer does not cover this URL."
                } else {
                    ""
                };
                found.push(
                    Finding::bad(
                        row.service.clone(),
                        format!("reachable directly from the internet at {}: {why}.{behind}", row.url),
                    )
                    .hint("Ingress internal-and-cloud-load-balancing closes it; if it is open on purpose, list it in platform.exposed."),
                );
            }
            (Reach::No, true) => found.push(
                Finding::warn(
                    row.service.clone(),
                    format!(
                        "listed in platform.exposed, but no longer exposed: {}",
                        row.closed_by.as_deref().unwrap_or("closed")
                    ),
                )
                .hint("Take it off the list, so the list stays honest."),
            ),
            _ => {}
        }
    }
    if snapshot.services().is_some() {
        for name in &snapshot.exposed {
            if !rows.iter().any(|r| &r.service == name) {
                found.push(
                    Finding::warn(name.clone(), "listed in platform.exposed, but no Cloud Run service has that name")
                        .hint("Take it off the list, so the list stays honest."),
                );
            }
        }
    }
    found
}

fn cert_findings(snapshot: &Snapshot, now: f64) -> Vec<Finding> {
    let mut found = Vec::new();
    for c in certificates(snapshot, now) {
        let hint = if c.in_use { None } else { Some("Nothing serves it.") };
        let mut flag = |what: String| {
            let finding = Finding::bad(c.name.clone(), what);
            found.push(match hint {
                Some(h) => finding.hint(h),
                None => finding,
            });
        };
        if c.not_active() {
            let status = if c.status.is_empty() { "without a status" } else { c.status.as_str() };
            let problems = if c.problems.is_empty() { String::new() } else { format!(": {}", c.problems.join(", ")) };
            flag(format!("managed certificate is {status}{problems}"));
        }
        if c.expiring(now) {
            flag(format!("certificate {}", c.expiry_text(now)));
        }
    }
    found
}

fn image_findings(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(Ok(repos)) = &snapshot.repos else { return Vec::new() };
    repos.iter().filter_map(|r| Some(Finding::warn(r.id.clone(), r.cleanup_problem()?))).collect()
}

/// A check's exit: 1 when something needs attention, whatever else could not be read; otherwise
/// 2 when something could not be read, 0 when all is clear.
pub fn check_exit(found: &[Finding], unread: &[Unread]) -> std::process::ExitCode {
    if !found.iter().any(|f| f.level == Level::Bad) && !unread.is_empty() {
        return std::process::ExitCode::from(findings::COULD_NOT_TELL);
    }
    findings::exit_code(found)
}

// ---- tables -------------------------------------------------------------------------------------

/// A row of a section, for the terminal UI and the commands alike.
#[derive(Debug, Clone)]
pub struct Row {
    pub cells: Vec<(String, Tone)>,
    /// Its name: what is copied.
    pub name: String,
    /// Its page in the Cloud console.
    pub console: String,
    /// What the list has no room for.
    pub detail: Vec<String>,
    /// Worth a look.
    pub flagged: bool,
    /// Belongs in a command's overview: a service only when the internet can get in.
    pub overview: bool,
}

pub struct Table {
    pub cols: Vec<Column>,
    pub rows: Vec<Row>,
}

fn col(head: &'static str, right: bool, drop: u8) -> Column {
    Column { head, right, drop }
}

impl Snapshot {
    /// A section's rows.
    pub fn table(&self, section: Section, now: f64) -> Table {
        match section {
            Section::Limits => self.limits_table(),
            Section::Orphans => self.orphans_table(),
            Section::Exposure => self.exposure_table(),
            Section::Certificates => self.certificates_table(now),
            Section::Images => self.images_table(),
        }
    }

    fn console(&self, path: &str) -> String {
        console_url(&self.project, path)
    }

    fn limits_table(&self) -> Table {
        let cols = vec![
            col("WHAT", false, 0),
            col("USED", true, 0),
            col("LIMIT", true, 0),
            col("%", true, 0),
            col("METRIC", false, 2),
            col("NOTES", false, 0),
        ];
        let rows = self.lb().map(Lb::limits).unwrap_or_default().into_iter().map(|l| {
            let tone = l.tone();
            let percent = l.percent().map(|p| format!("{p}%")).unwrap_or_default();
            let limit = l.limit.map_or("—".to_string(), |v| v.to_string());
            let note = l.note();
            let mut detail = Vec::new();
            if !l.metric.is_empty() {
                let reports = l.reported.map(|r| format!(", which reports {r}")).unwrap_or_default();
                detail.push(format!("Quota metric {}{reports}", l.metric));
            }
            if !note.is_empty() {
                detail.push(note.clone());
            }
            Row {
                name: l.what.clone(),
                console: self.console("iam-admin/quotas"),
                detail,
                flagged: matches!(tone, Tone::Bad | Tone::Warn),
                overview: true,
                cells: vec![
                    (l.what.clone(), Tone::Plain),
                    (l.used.to_string(), tone),
                    (limit, Tone::Muted),
                    (percent, tone),
                    (l.metric.clone(), Tone::Muted),
                    (note, if l.counted() { Tone::Warn } else { Tone::Muted }),
                ],
            }
        });
        Table { rows: rows.collect(), cols }
    }

    fn orphans_table(&self) -> Table {
        let cols = vec![col("KIND", false, 0), col("NAME", false, 0), col("WHERE", false, 2), col("WHY", false, 0)];
        let list = self.lb().map(|lb| orphans(lb, self.services())).unwrap_or_default();
        let rows = list.into_iter().map(|o| {
            let page = match o.kind {
                "NEG" => "compute/networkendpointgroups/list",
                "backend service" => "net-services/loadbalancing/list/backends",
                _ => "net-services/loadbalancing/list/loadBalancers",
            };
            Row {
                console: self.console(page),
                detail: vec![format!("{} {} in {}", o.kind, o.name, o.place), o.why.join("; ")],
                flagged: true,
                overview: true,
                name: o.name.clone(),
                cells: vec![
                    (o.kind.to_string(), Tone::Muted),
                    (o.name, Tone::Warn),
                    (o.place, Tone::Muted),
                    (o.why.join("; "), Tone::Plain),
                ],
            }
        });
        Table { rows: rows.collect(), cols }
    }

    fn exposure_table(&self) -> Table {
        let cols = vec![
            col("SERVICE", false, 0),
            col("INGRESS", false, 3),
            col("LB", false, 1),
            col("IAP", false, 2),
            col("INVOKER", false, 2),
            col("DIRECT", false, 0),
            col("NOTES", false, 0),
        ];
        let rows = exposure(self).into_iter().map(|e| {
            let (direct, direct_tone) = match (&e.reach, e.listed) {
                (Reach::Yes(_), false) => ("yes".to_string(), Tone::Bad),
                (Reach::Yes(_), true) => ("yes, listed".to_string(), Tone::Warn),
                (Reach::Unknown, _) => ("?".to_string(), Tone::Warn),
                (Reach::No, true) => ("no, listed".to_string(), Tone::Warn),
                (Reach::No, false) => ("no".to_string(), Tone::Muted),
            };
            let lb = match (e.routes_known, e.routes.is_empty()) {
                (false, _) => "?",
                (true, true) => "no",
                (true, false) => "yes",
            };
            let (note, note_tone) = e.note(&self.region);
            let mut detail = vec![format!("{} · {} · ingress {}", e.region, e.url, e.ingress)];
            for r in &e.routes {
                let armor = r.armor.as_ref().map(|a| format!(", Cloud Armor {a}")).unwrap_or_default();
                let gate = if r.iap { "IAP" } else { "public" };
                detail.push(format!("Backend {}: {gate}{armor}", r.backend));
            }
            if let Some(why) = &e.closed_by {
                detail.push(format!("run.app closed: {why}"));
            }
            if !note.is_empty() {
                detail.push(note.clone());
            }
            Row {
                flagged: e.rank() <= 2,
                overview: e.ingress == "all" || e.listed,
                name: e.service.clone(),
                console: e.console.clone(),
                detail,
                cells: vec![
                    (e.service.clone(), Tone::Plain),
                    (e.ingress.clone(), Tone::Muted),
                    (lb.to_string(), Tone::Muted),
                    (e.gate().to_string(), Tone::Muted),
                    (
                        e.invoker_text(),
                        if matches!(e.invoker, Some(Invoker::Public { .. })) { direct_tone } else { Tone::Muted },
                    ),
                    (direct, direct_tone),
                    (note, note_tone),
                ],
            }
        });
        Table { rows: rows.collect(), cols }
    }

    fn certificates_table(&self, now: f64) -> Table {
        let cols = vec![
            col("NAME", false, 0),
            col("FROM", false, 3),
            col("TYPE", false, 4),
            col("STATUS", false, 0),
            col("EXPIRES", true, 0),
            col("IN USE", false, 2),
            col("DOMAINS", false, 0),
        ];
        let rows = certificates(self, now).into_iter().map(|c| {
            let days = c.days_left(now).map(|d| format!("{d:.0}d")).unwrap_or_else(|| "—".to_string());
            let expiry_tone = if c.expiring(now) { Tone::Bad } else { Tone::Muted };
            let status_tone = if c.not_active() { Tone::Bad } else { Tone::Plain };
            let status = if c.status.is_empty() { "—".to_string() } else { c.status.clone() };
            let mut detail = vec![c.domains.join(", ")];
            if c.expires.is_some() {
                detail.push(c.expiry_text(now));
            }
            detail.extend(c.problems.iter().cloned());
            Row {
                flagged: c.not_active() || c.expiring(now),
                overview: true,
                name: c.name.clone(),
                console: self.console(if c.source == "compute" {
                    "net-services/loadbalancing/advanced/sslCertificates/list"
                } else {
                    "security/ccm/list/lbCertificates"
                }),
                detail,
                cells: vec![
                    (c.name.clone(), Tone::Plain),
                    (c.source.to_string(), Tone::Muted),
                    ((if c.managed { "managed" } else { "self-managed" }).to_string(), Tone::Muted),
                    (status, status_tone),
                    (days, expiry_tone),
                    ((if c.in_use { "yes" } else { "no" }).to_string(), Tone::Muted),
                    (c.domains.join(", "), Tone::Muted),
                ],
            }
        });
        Table { rows: rows.collect(), cols }
    }

    fn images_table(&self) -> Table {
        let cols = vec![
            col("REPOSITORY", false, 0),
            col("WHERE", false, 3),
            col("FORMAT", false, 4),
            col("SIZE", true, 0),
            col("CLEANUP", false, 0),
        ];
        let empty = Vec::new();
        let repos = match &self.repos {
            Some(Ok(list)) => list,
            _ => &empty,
        };
        let rows = repos.iter().map(|r| {
            let problem = r.cleanup_problem();
            let tone =
                if problem.is_some() || matches!(r.cleanup, Cleanup::Unread(_)) { Tone::Warn } else { Tone::Muted };
            let mut detail = vec![format!("{} · {} · {}", r.location, r.format, r.mode)];
            detail.extend(problem.clone());
            Row {
                flagged: problem.is_some(),
                overview: true,
                name: r.id.clone(),
                console: self.console(&format!(
                    "artifacts/{}/{}/{}/{}",
                    r.format.to_lowercase(),
                    self.project,
                    r.location,
                    r.id
                )),
                detail,
                cells: vec![
                    (r.id.clone(), Tone::Plain),
                    (r.location.clone(), Tone::Muted),
                    (r.format.clone(), Tone::Muted),
                    (bytes(r.size), Tone::Plain),
                    (r.policy_text(), tone),
                ],
            }
        });
        Table { rows: rows.collect(), cols }
    }

    /// The sections asked for, as `--json` and the MCP server give them.
    pub fn json(&self, env: &str, want: Want, now: f64) -> Value {
        let mut out =
            json!({ "env": env, "project": self.project, "region": Some(&self.region).filter(|r| !r.is_empty()) });
        let lb = self.lb();
        if want.limits {
            out["limits"] = json!(lb.map(Lb::limits).unwrap_or_default().iter().map(Limit::json).collect::<Vec<_>>());
        }
        if want.orphans {
            let list = lb.map(|lb| orphans(lb, self.services())).unwrap_or_default();
            out["orphans"] = json!(list.iter().map(Orphan::json).collect::<Vec<_>>());
        }
        if want.exposure {
            out["exposure"] = json!(exposure(self).iter().map(Exposure::json).collect::<Vec<_>>());
            out["exposedList"] = json!(self.exposed);
        }
        if want.certificates {
            out["certificates"] = json!(certificates(self, now).iter().map(|c| c.json(now)).collect::<Vec<_>>());
        }
        if want.images {
            let repos = self.repos.as_ref().and_then(|r| r.as_ref().ok());
            out["images"] = json!(repos.into_iter().flatten().map(Repo::json).collect::<Vec<_>>());
        }
        let mut found: Vec<Finding> = wanted(want).iter().flat_map(|s| section_findings(self, *s, now)).collect();
        findings::sort(&mut found);
        out["findings"] = json!(found);
        out["unread"] = json!(self.unread().iter().map(unread_json).collect::<Vec<_>>());
        let notes = self.notes();
        if !notes.is_empty() {
            out["notes"] = json!(notes);
        }
        out
    }
}

fn unread_json(u: &Unread) -> Value {
    json!({ "what": u.what, "error": u.error.short() })
}

/// The sections `want` holds.
pub fn wanted(want: Want) -> Vec<Section> {
    let on = [want.limits, want.orphans, want.exposure, want.certificates, want.images];
    Section::ALL.iter().zip(on).filter(|(_, on)| *on).map(|(s, _)| *s).collect()
}

#[cfg(test)]
pub(crate) mod tests;
