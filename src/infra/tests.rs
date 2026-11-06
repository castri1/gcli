use serde_json::json;

use super::*;
use crate::gcp::auth;
use crate::gcp::mock::{MockServer, Reply};

const NOW: f64 = 1_793_923_200.0; // 2026-11-06 00:00 UTC

const COMPUTE: &str = "https://www.googleapis.com/compute/v1/projects/my-platform-123456";

fn backend_url(name: &str) -> String {
    format!("{COMPUTE}/global/backendServices/{name}")
}

fn neg_url(name: &str) -> String {
    format!("{COMPUTE}/regions/us-east1/networkEndpointGroups/{name}")
}

fn backend(name: &str, neg: Option<&str>) -> BackendService {
    serde_json::from_value(json!({
        "name": name,
        "selfLink": backend_url(name),
        "loadBalancingScheme": "EXTERNAL_MANAGED",
        "protocol": "HTTPS",
        "backends": neg.map(|n| vec![json!({ "group": neg_url(n) })]).unwrap_or_default(),
    }))
    .unwrap()
}

fn neg(name: &str, service: &str) -> NetworkEndpointGroup {
    serde_json::from_value(json!({
        "name": name,
        "selfLink": neg_url(name),
        "region": format!("{COMPUTE}/regions/us-east1"),
        "networkEndpointType": "SERVERLESS",
        "cloudRun": { "service": service },
    }))
    .unwrap()
}

/// A URL map serving each (host, backend).
fn url_map(name: &str, hosts: &[(&str, &str)]) -> UrlMap {
    serde_json::from_value(json!({
        "name": name,
        "selfLink": format!("{COMPUTE}/global/urlMaps/{name}"),
        "hostRules": hosts.iter().map(|(h, b)| json!({ "hosts": [h], "pathMatcher": b })).collect::<Vec<_>>(),
        "pathMatchers": hosts.iter().map(|(_, b)| json!({ "name": b, "defaultService": backend_url(b) })).collect::<Vec<_>>(),
    }))
    .unwrap()
}

/// A proxy and a forwarding rule in front of URL map `name`.
fn served(lb: &mut Lb, name: &str) {
    let proxy = format!("{COMPUTE}/global/targetHttpsProxies/{name}-proxy");
    lb.proxies.push(TargetProxy {
        self_link: proxy.clone(),
        url_map: format!("{COMPUTE}/global/urlMaps/{name}"),
        ssl_certificates: Vec::new(),
    });
    lb.rules.push(ForwardingRule { target: proxy, backend_service: String::new() });
}

fn service(id: &str, ingress: &str) -> Service {
    serde_json::from_value(json!({
        "name": format!("projects/my-platform-123456/locations/us-east1/services/{id}"),
        "uri": format!("https://{id}-abc123-ue.a.run.app"),
        "ingress": ingress,
    }))
    .unwrap()
}

fn quota_list(rows: &[(&str, u64, u64)]) -> Vec<Quota> {
    rows.iter().map(|(m, u, l)| Quota { metric: m.to_string(), usage: *u as f64, limit: *l as f64 }).collect()
}

fn known(list: &[Service]) -> Services {
    Services { list: list.to_vec(), unreachable: Vec::new() }
}

fn snapshot(lb: Lb, services: Vec<Service>) -> Snapshot {
    Snapshot {
        project: "my-platform-123456".into(),
        region: "us-east1".into(),
        lb: Some(Ok(lb)),
        services: Some(Ok(known(&services))),
        ..Snapshot::default()
    }
}

fn invokers(rows: &[(&str, gcp::Result<Invoker>)]) -> HashMap<String, gcp::Result<Invoker>> {
    rows.iter()
        .map(|(id, r)| (format!("projects/my-platform-123456/locations/us-east1/services/{id}"), r.clone()))
        .collect()
}

fn public() -> Invoker {
    Invoker::Public { member: "allUsers".into(), role: "roles/run.invoker".into() }
}

fn denied_error() -> GcpError {
    GcpError::Api { status: 403, code: "PERMISSION_DENIED".into(), message: "no".into() }
}

// ---- limits ---------------------------------------------------------------------------------

#[test]
fn the_backend_quota_is_the_metric_of_their_scheme_not_the_one_named_backend_services() {
    let lb = Lb {
        quotas: quota_list(&[
            ("BACKEND_SERVICES", 0, 200),
            ("GLOBAL_EXTERNAL_MANAGED_BACKEND_SERVICES", 3, 200),
            ("GLOBAL_INTERNAL_MANAGED_BACKEND_SERVICES", 0, 200),
        ]),
        backends: vec![backend("a", None), backend("b", None), backend("c", None)],
        ..Lb::default()
    };
    let rows = lb.backend_limits();
    assert_eq!(rows.len(), 1, "metrics at zero with nothing of theirs stay out: {rows:?}");
    assert_eq!(rows[0].metric, "GLOBAL_EXTERNAL_MANAGED_BACKEND_SERVICES");
    assert_eq!((rows[0].used, rows[0].limit, rows[0].percent()), (3, Some(200), Some(1)));
    assert!(!rows[0].counted());
    assert_eq!(rows[0].note(), "");
}

#[test]
fn a_metric_that_reports_nothing_gives_way_to_the_count_and_says_so() {
    let lb = Lb {
        quotas: quota_list(&[("GLOBAL_EXTERNAL_MANAGED_BACKEND_SERVICES", 0, 4)]),
        backends: (0..4).map(|i| backend(&format!("b{i}"), None)).collect(),
        ..Lb::default()
    };
    let rows = lb.backend_limits();
    assert_eq!((rows[0].used, rows[0].reported, rows[0].listed), (4, Some(0), Some(4)));
    assert!(rows[0].counted());
    assert_eq!(rows[0].note(), "4 counted from the list: the metric reports 0");
    let found = limit_findings(&lb);
    assert_eq!(found[0].level, Level::Bad);
    assert!(found[0].what.contains("counted from the list"), "{}", found[0].what);
}

#[test]
fn a_metric_the_project_does_not_have_falls_back_on_the_broadest_limit() {
    let lb =
        Lb { quotas: quota_list(&[("BACKEND_SERVICES", 0, 200)]), backends: vec![backend("a", None)], ..Lb::default() };
    let rows = lb.backend_limits();
    assert_eq!(rows[0].metric, "GLOBAL_EXTERNAL_MANAGED_BACKEND_SERVICES");
    assert_eq!((rows[0].used, rows[0].limit), (1, Some(200)));
    assert!(rows[0].counted());
    // No limit anywhere: the count alone, and it says so.
    let lb = Lb { backends: vec![backend("a", None)], ..Lb::default() };
    assert_eq!(lb.backend_limits()[0].limit, None);
    assert_eq!(lb.backend_limits()[0].note(), "no limit in the API");
    assert!(limit_findings(&lb).is_empty());
}

#[test]
fn every_scheme_has_its_own_metric() {
    let of = |scheme: &str, protocol: &str, region: &str| {
        backend_metric(&BackendService {
            load_balancing_scheme: scheme.into(),
            protocol: protocol.into(),
            region: region.into(),
            ..BackendService::default()
        })
    };
    assert_eq!(of("EXTERNAL_MANAGED", "HTTPS", ""), "GLOBAL_EXTERNAL_MANAGED_BACKEND_SERVICES");
    assert_eq!(of("EXTERNAL", "HTTP", ""), "BACKEND_SERVICES");
    assert_eq!(of("EXTERNAL", "TCP", ""), "GLOBAL_EXTERNAL_PROXY_LB_BACKEND_SERVICES");
    assert_eq!(of("EXTERNAL_MANAGED", "SSL", ""), "GLOBAL_EXTERNAL_PROXY_LB_BACKEND_SERVICES");
    assert_eq!(of("INTERNAL_MANAGED", "HTTP", ""), "GLOBAL_INTERNAL_MANAGED_BACKEND_SERVICES");
    assert_eq!(of("INTERNAL_SELF_MANAGED", "HTTP", ""), "GLOBAL_INTERNAL_TRAFFIC_DIRECTOR_BACKEND_SERVICES");
    assert_eq!(of("INTERNAL", "TCP", "https://x/regions/r"), "REGIONAL_INTERNAL_LB_BACKEND_SERVICES");
    assert_eq!(of("EXTERNAL_MANAGED", "HTTP", "https://x/regions/r"), "REGIONAL_EXTERNAL_MANAGED_BACKEND_SERVICES");
}

#[test]
fn the_backend_quota_warns_at_80_and_is_bad_at_95() {
    for (used, level) in [
        (159, None),
        (160, Some(Level::Warn)),
        (189, Some(Level::Warn)),
        (190, Some(Level::Bad)),
        (200, Some(Level::Bad)),
    ] {
        let lb = Lb { quotas: quota_list(&[("GLOBAL_EXTERNAL_MANAGED_BACKEND_SERVICES", used, 200)]), ..Lb::default() };
        let found = limit_findings(&lb);
        assert_eq!(found.first().map(|f| f.level), level, "{used} of 200: {found:?}");
        if level.is_some() {
            assert_eq!(found[0].subject, "backend services");
            assert!(found[0].what.contains(&format!("{used} of 200")), "{}", found[0].what);
            assert!(found[0].what.contains("GLOBAL_EXTERNAL_MANAGED_BACKEND_SERVICES"));
        }
    }
}

#[test]
fn negs_and_what_is_in_a_url_map_are_shown_against_what_limits_them() {
    let lb = Lb {
        quotas: quota_list(&[("NETWORK_ENDPOINT_GROUPS", 2, 1000), ("URL_MAPS", 1, 100), ("SECURITY_POLICIES", 0, 10)]),
        regions: vec![Region { name: "us-east1".into(), quotas: quota_list(&[("NETWORK_ENDPOINT_GROUPS", 0, 1000)]) }],
        negs: vec![neg("a", "a"), neg("b", "b"), neg("c", "c")],
        policies: vec![SecurityPolicy { name: "armor".into() }],
        url_maps: vec![url_map("main", &[("a.example.com", "a"), ("b.example.com", "b")])],
        ..Lb::default()
    };
    let rows = lb.limits();
    let row = |what: &str| rows.iter().find(|r| r.what == what).unwrap_or_else(|| panic!("{what} in {rows:?}"));
    assert_eq!((row("NEGs").used, row("NEGs").limit), (3, Some(1000)));
    assert_eq!(row("NEGs").note(), "3 counted from the list: the metric reports 2");
    assert_eq!(row("NEGs in us-east1").note(), "3 counted from the list: the metric reports 0");
    assert_eq!(row("URL maps").used, 1);
    assert_eq!(row("Security policies").used, 1, "a metric behind the list");
    assert_eq!((row("Host rules in main").used, row("Host rules in main").limit), (2, None));
    assert_eq!(row("Path matchers in main").note(), "no limit in the API");
    assert_eq!(row("Path rules in main").used, 0);
    // A metric that counts more than the regions read is believed, and says why.
    let lb =
        Lb { quotas: quota_list(&[("NETWORK_ENDPOINT_GROUPS", 9, 1000)]), negs: vec![neg("a", "a")], ..Lb::default() };
    assert_eq!(lb.neg_limits()[0].used, 9);
    assert_eq!(lb.neg_limits()[0].note(), "9 by the metric: 1 found in the regions read");
}

// ---- orphans --------------------------------------------------------------------------------

fn why(list: &[Orphan], name: &str) -> Vec<String> {
    list.iter().find(|o| o.name == name).map(|o| o.why.clone()).unwrap_or_default()
}

#[test]
fn each_kind_of_orphan_is_derived_from_what_points_to_what() {
    let mut lb = Lb {
        backends: vec![
            backend("shop-backend", Some("shop-neg")),
            // Nothing maps it, and its NEG leads to a service that is gone.
            backend("gone-backend", Some("gone-neg")),
            // Mapped, and empty.
            backend("empty-backend", None),
            // Mapped, its service gone: the host leads nowhere.
            backend("stale-backend", Some("stale-neg")),
        ],
        negs: vec![
            neg("shop-neg", "shop"),
            neg("gone-neg", "gone"),
            neg("stale-neg", "stale"),
            neg("unused-neg", "shop"),
        ],
        url_maps: vec![url_map(
            "main",
            &[
                ("shop.example.com", "shop-backend"),
                ("empty.example.com", "empty-backend"),
                ("stale.example.com", "stale-backend"),
            ],
        )],
        ..Lb::default()
    };
    served(&mut lb, "main");
    let list = orphans(&lb, Some(&known(&[service("shop", INGRESS_LB)])));
    assert_eq!(why(&list, "gone-backend"), ["no URL map uses it"]);
    assert_eq!(why(&list, "empty-backend"), ["it has no backends"]);
    assert_eq!(why(&list, "gone-neg"), ["its Cloud Run service gone does not exist"]);
    assert_eq!(why(&list, "stale-neg"), ["its Cloud Run service stale does not exist"]);
    assert_eq!(why(&list, "unused-neg"), ["no backend uses it"]);
    assert_eq!(why(&list, "stale.example.com"), ["its Cloud Run service stale does not exist"]);
    assert_eq!(list.iter().find(|o| o.name == "stale.example.com").unwrap().place, "main");
    assert_eq!(list.len(), 6, "{list:?}");
    assert!(why(&list, "shop-backend").is_empty() && why(&list, "shop-neg").is_empty());
    assert!(why(&list, "empty.example.com").is_empty(), "a host on an empty backend says nothing of a service");

    // One NEG, two reasons: counted once.
    let lb = Lb { negs: vec![neg("gone-neg", "gone")], ..Lb::default() };
    let list = orphans(&lb, Some(&NO_SERVICES));
    assert_eq!(list.len(), 1);
    assert_eq!(why(&list, "gone-neg"), ["no backend uses it", "its Cloud Run service gone does not exist"]);
}

#[test]
fn a_service_by_that_name_in_another_region_does_not_count() {
    let lb = Lb { backends: vec![backend("b", Some("n"))], negs: vec![neg("n", "shop")], ..Lb::default() };
    let mut elsewhere = service("shop", INGRESS_LB);
    elsewhere.name = "projects/my-platform-123456/locations/europe-west1/services/shop".into();
    let list = orphans(&lb, Some(&known(&[elsewhere])));
    assert_eq!(why(&list, "n"), ["its Cloud Run service shop does not exist"]);
    // A region that could not be asked may hold it.
    let unsure = Services { list: Vec::new(), unreachable: vec!["us-east1".into()] };
    assert!(why(&orphans(&lb, Some(&unsure)), "n").is_empty());
}

#[test]
fn what_is_not_an_orphan() {
    // A backend bucket is not a backend service, and a host behind one says nothing of a service.
    let mut map = url_map("main", &[("site.example.com", "site-backend")]);
    map.path_matchers[0].default_service = format!("{COMPUTE}/global/backendBuckets/site-bucket");
    // A TCP proxy uses a backend without a URL map; so does a passthrough rule.
    let mut tcp = backend("tcp-backend", Some("tcp-neg"));
    tcp.used_by = vec![compute::UsedBy { reference: format!("{COMPUTE}/global/targetTcpProxies/p") }];
    let passthrough = backend("direct-backend", Some("direct-neg"));
    // A URL mask names no service, and another kind of NEG has none to look for.
    let mut mask = neg("mask-neg", "");
    mask.cloud_run.url_mask = "<service>.example.com".into();
    let mut attachment = neg("psc-neg", "");
    attachment.network_endpoint_type = "PRIVATE_SERVICE_CONNECT".into();
    let lb = Lb {
        url_maps: vec![map],
        backends: vec![
            tcp,
            passthrough,
            {
                let mut b = backend("mask-backend", Some("mask-neg"));
                b.used_by = vec![compute::UsedBy { reference: "x".into() }];
                b
            },
            {
                let mut b = backend("psc-backend", Some("psc-neg"));
                b.used_by = vec![compute::UsedBy { reference: "x".into() }];
                b
            },
        ],
        rules: vec![ForwardingRule { target: String::new(), backend_service: backend_url("direct-backend") }],
        negs: vec![neg("tcp-neg", "tcp"), neg("direct-neg", "direct"), mask, attachment],
        ..Lb::default()
    };
    let services = known(&[service("tcp", INGRESS_LB), service("direct", INGRESS_LB)]);
    let list = orphans(&lb, Some(&services));
    assert!(list.is_empty(), "{list:?}");
}

#[test]
fn services_that_could_not_be_read_never_read_as_deleted() {
    let lb = Lb {
        backends: vec![backend("b", Some("n"))],
        negs: vec![neg("n", "shop")],
        url_maps: vec![url_map("main", &[("shop.example.com", "b")])],
        ..Lb::default()
    };
    assert!(orphans(&lb, None).is_empty());
    let mut s = snapshot(lb, vec![]);
    s.services = Some(Err(denied_error()));
    assert!(s.table(Section::Orphans, NOW).rows.is_empty());
    assert!(section_findings(&s, Section::Orphans, NOW).is_empty());
    assert_eq!(s.unread().len(), 1);
    // The API being off means there are none, and what led to one is a leftover.
    s.services = Some(Err(GcpError::Api {
        status: 403,
        code: "PERMISSION_DENIED".into(),
        message: "Cloud Run Admin API has not been used in project p before or it is disabled.".into(),
    }));
    assert!(s.unread().is_empty());
    assert_eq!(s.table(Section::Orphans, NOW).rows.len(), 2, "the NEG and the host rule");
    assert!(s.notes()[0].contains("Cloud Run"));
}

#[test]
fn orphans_warn_with_names_and_are_bad_above_a_tenth_of_the_backend_quota() {
    let quotas = quota_list(&[("GLOBAL_EXTERNAL_MANAGED_BACKEND_SERVICES", 100, 200)]);
    let build = |n: usize| Lb {
        quotas: quotas.clone(),
        negs: (0..n).map(|i| neg(&format!("left-{i:02}"), "shop")).collect(),
        ..Lb::default()
    };
    let services = known(&[service("shop", INGRESS_LB)]);
    assert!(orphan_findings(&build(0), Some(&services)).is_empty());

    let found = orphan_findings(&build(2), Some(&services));
    assert_eq!((found.len(), found[0].level), (1, Level::Warn));
    assert_eq!(found[0].subject, "orphans");
    assert_eq!(found[0].what, "2 left behind: NEG left-00, NEG left-01");

    // 20 of 200 is exactly a tenth, not more.
    let found = orphan_findings(&build(20), Some(&services));
    assert_eq!(found[0].level, Level::Warn);
    assert!(found[0].what.starts_with("20 left behind: NEG left-00"), "{}", found[0].what);
    assert!(found[0].what.ends_with("and 10 more"), "{}", found[0].what);

    let found = orphan_findings(&build(21), Some(&services));
    assert_eq!(found[0].level, Level::Bad);
    assert!(found[0].what.contains("more than 10% of the backend quota of 200"), "{}", found[0].what);
}

// ---- exposure -------------------------------------------------------------------------------

fn only<'a>(rows: &'a [Exposure], id: &str) -> &'a Exposure {
    rows.iter().find(|r| r.service == id).unwrap_or_else(|| panic!("{id} in {rows:?}"))
}

/// `shop`: behind IAP and, for its public paths, not; `open`: public only; `off`: mapped by nothing live.
fn routed() -> Lb {
    let mut public_one = backend("shop-public-backend", Some("shop-public-neg"));
    public_one.security_policy = format!("{COMPUTE}/global/securityPolicies/shop-armor");
    let mut iap_one = backend("shop-backend", Some("shop-neg"));
    iap_one.iap.enabled = true;
    let mut lb = Lb {
        backends: vec![
            iap_one,
            public_one,
            backend("open-backend", Some("open-neg")),
            backend("off-backend", Some("off-neg")),
        ],
        negs: vec![
            neg("shop-neg", "shop"),
            neg("shop-public-neg", "shop"),
            neg("open-neg", "open"),
            neg("off-neg", "off"),
        ],
        url_maps: vec![
            url_map("main", &[("shop.example.com", "shop-backend"), ("open.example.com", "open-backend")]),
            url_map("unserved", &[("off.example.com", "off-backend")]),
        ],
        ..Lb::default()
    };
    lb.url_maps[0].path_matchers[0]
        .path_rules
        .push(json!({ "paths": ["/api/*"], "service": backend_url("shop-public-backend") }));
    served(&mut lb, "main");
    lb
}

#[test]
fn a_load_balancer_reaches_what_a_live_chain_leads_to() {
    let s = snapshot(
        routed(),
        vec![
            service("shop", INGRESS_LB),
            service("open", INGRESS_LB),
            service("off", INGRESS_LB),
            service("alone", INGRESS_LB),
        ],
    );
    let rows = exposure(&s);
    let shop = only(&rows, "shop");
    assert_eq!(shop.gate(), "iap + public");
    assert_eq!(shop.routes.len(), 2);
    assert_eq!(shop.routes.iter().find(|r| !r.iap).unwrap().armor.as_deref(), Some("shop-armor"));
    assert!(shop.note("us-east1").0.contains("public paths split off: shop-public-backend without IAP"));
    assert_eq!(only(&rows, "open").gate(), "public");
    assert_eq!(only(&rows, "off").gate(), "none", "a URL map no forwarding rule reaches is not a way in");
    assert_eq!(only(&rows, "alone").gate(), "none");
    // Unread pieces are said to be unknown, never that nothing reaches it.
    let mut s = s.clone();
    s.lb = Some(Err(denied_error()));
    let rows = exposure(&s);
    assert_eq!(only(&rows, "shop").gate(), "?");
    assert!(!only(&rows, "shop").routes_known);
}

#[test]
fn the_dangerous_cell_is_ingress_all_and_anyone_may_invoke() {
    let mut no_check = service("no-check", INGRESS_ALL);
    no_check.invoker_iam_disabled = true;
    let mut no_url = service("no-url", INGRESS_ALL);
    no_url.default_uri_disabled = true;
    let mut own_iap = service("own-iap", INGRESS_ALL);
    own_iap.iap_enabled = true;
    let services = vec![
        service("anyone", INGRESS_ALL),
        service("accounts", INGRESS_ALL),
        service("iam-only", INGRESS_ALL),
        service("behind-lb", INGRESS_LB),
        service("inside", INGRESS_INTERNAL),
        no_check,
        no_url,
        own_iap,
    ];
    let mut s = snapshot(Lb::default(), services);
    s.invokers = Some(invokers(&[
        ("anyone", Ok(public())),
        ("accounts", Ok(Invoker::Public { member: "allAuthenticatedUsers".into(), role: "roles/run.invoker".into() })),
        ("iam-only", Ok(Invoker::Private)),
        ("no-check", Ok(Invoker::Private)),
        ("no-url", Ok(public())),
        ("own-iap", Ok(public())),
    ]));
    let rows = exposure(&s);
    let reach = |id: &str| only(&rows, id).reach.clone();
    assert_eq!(reach("anyone"), Reach::Yes("ingress all and allUsers holds roles/run.invoker".into()));
    assert_eq!(reach("accounts"), Reach::Yes("ingress all and allAuthenticatedUsers holds roles/run.invoker".into()));
    assert_eq!(reach("no-check"), Reach::Yes("ingress all and the invoker check is off (invokerIamDisabled)".into()));
    for closed in ["iam-only", "behind-lb", "inside", "no-url", "own-iap"] {
        assert_eq!(reach(closed), Reach::No, "{closed}");
    }
    assert_eq!(only(&rows, "behind-lb").closed_by.as_deref(), Some("ingress internal-and-cloud-load-balancing"));
    assert_eq!(only(&rows, "inside").ingress, "internal");
    assert_eq!(only(&rows, "no-url").closed_by.as_deref(), Some("its run.app URL is turned off"));

    let found = exposure_findings(&s);
    let bad: Vec<&str> = found.iter().filter(|f| f.level == Level::Bad).map(|f| f.subject.as_str()).collect();
    assert_eq!(found.len(), 3, "{found:?}");
    for name in ["anyone", "accounts", "no-check"] {
        assert!(bad.contains(&name), "{name} in {bad:?}");
    }
    let anyone = found.iter().find(|f| f.subject == "anyone").unwrap();
    assert!(anyone.what.contains("https://anyone-abc123-ue.a.run.app"), "{}", anyone.what);
    assert!(anyone.what.contains("allUsers holds roles/run.invoker"), "names what makes it so: {}", anyone.what);
    // The worst come first in the list.
    assert!(matches!(rows[0].reach, Reach::Yes(_)));
}

#[test]
fn iap_on_the_load_balancer_does_not_cover_the_run_app_url() {
    let mut s = snapshot(routed(), vec![service("shop", INGRESS_ALL)]);
    s.invokers = Some(invokers(&[("shop", Ok(public()))]));
    let found = exposure_findings(&s);
    assert_eq!(found[0].level, Level::Bad);
    assert!(found[0].what.contains("IAP on its load balancer does not cover this URL"), "{}", found[0].what);
}

#[test]
fn who_may_invoke_is_read_from_the_policy() {
    let policy =
        |bindings: serde_json::Value| -> IamPolicy { serde_json::from_value(json!({ "bindings": bindings })).unwrap() };
    assert_eq!(invoker_of(&IamPolicy::default()), Invoker::Private);
    assert_eq!(
        invoker_of(&policy(json!([{ "role": "roles/run.invoker", "members": ["user:ana@example.com"] }]))),
        Invoker::Private
    );
    assert_eq!(invoker_of(&policy(json!([{ "role": "roles/run.invoker", "members": ["allUsers"] }]))), public());
    // Viewing is not invoking, and a custom role may be.
    assert_eq!(invoker_of(&policy(json!([{ "role": "roles/run.viewer", "members": ["allUsers"] }]))), Invoker::Private);
    assert_eq!(
        invoker_of(&policy(json!([{ "role": "organizations/1/roles/callers", "members": ["allAuthenticatedUsers"] }]))),
        Invoker::Public { member: "allAuthenticatedUsers".into(), role: "organizations/1/roles/callers".into() }
    );
    // The broader of the two is what is said.
    assert_eq!(
        invoker_of(&policy(json!([
            { "role": "roles/run.invoker", "members": ["allAuthenticatedUsers"] },
            { "role": "roles/run.admin", "members": ["allUsers"] }
        ]))),
        Invoker::Public { member: "allUsers".into(), role: "roles/run.admin".into() }
    );
}

#[test]
fn a_listed_app_is_not_a_finding_while_exposed_and_a_warning_once_it_is_not() {
    let mut s = snapshot(
        Lb::default(),
        vec![service("webhooks", INGRESS_ALL), service("closed", INGRESS_LB), service("iam-only", INGRESS_ALL)],
    );
    s.exposed = vec!["webhooks".into(), "closed".into(), "iam-only".into(), "deleted".into()];
    s.invokers = Some(invokers(&[("webhooks", Ok(public())), ("iam-only", Ok(Invoker::Private))]));
    let found = exposure_findings(&s);
    assert!(found.iter().all(|f| f.level == Level::Warn), "{found:?}");
    assert!(!found.iter().any(|f| f.subject == "webhooks"), "{found:?}");
    let on = |id: &str| found.iter().find(|f| f.subject == id).unwrap().what.clone();
    assert_eq!(
        on("closed"),
        "listed in platform.exposed, but no longer exposed: ingress internal-and-cloud-load-balancing"
    );
    assert_eq!(on("iam-only"), "listed in platform.exposed, but no longer exposed: only IAM members may invoke it");
    assert_eq!(on("deleted"), "listed in platform.exposed, but no Cloud Run service has that name");
    // Still shown as listed.
    let table = s.table(Section::Exposure, NOW);
    let cells = |id: &str| table.rows.iter().find(|r| r.name == id).unwrap().cells.clone();
    assert_eq!(cells("webhooks")[5].0, "yes, listed");
    assert!(cells("webhooks")[6].0.starts_with("listed: ingress all and allUsers"));
    assert_eq!(cells("closed")[5].0, "no, listed");
    assert!(!table.rows.iter().find(|r| r.name == "webhooks").unwrap().flagged);
    assert!(table.rows.iter().find(|r| r.name == "closed").unwrap().flagged);
    // Without the services the list cannot be judged.
    s.services = Some(Err(denied_error()));
    assert!(exposure_findings(&s).is_empty());
}

#[test]
fn a_policy_that_could_not_be_read_is_unknown_never_safe() {
    let mut s = snapshot(Lb::default(), vec![service("a", INGRESS_ALL), service("b", INGRESS_ALL)]);
    s.exposed = vec!["b".into()];
    s.invokers = Some(invokers(&[("a", Err(denied_error())), ("b", Err(denied_error()))]));
    let rows = exposure(&s);
    assert_eq!(only(&rows, "a").reach, Reach::Unknown);
    assert!(exposure_findings(&s).is_empty(), "neither exposed nor no longer exposed");
    let unread = s.unread();
    assert_eq!(unread.len(), 1);
    assert_eq!(unread[0].what, "who may invoke a, b");
    assert_eq!(s.table(Section::Exposure, NOW).rows[0].cells[5].0, "?");
    assert_eq!(Exposure::json(&rows[0])["direct"], Value::Null);
}

// ---- certificates ---------------------------------------------------------------------------

fn manager_cert(state: &str, expires: &str) -> Certificate {
    serde_json::from_value(json!({
        "name": "projects/p/locations/global/certificates/wild",
        "sanDnsnames": ["*.example.com"],
        "expireTime": expires,
        "managed": { "state": state, "domains": ["*.example.com"],
                     "authorizationAttemptInfo": [{ "domain": "*.example.com", "state": "FAILED", "failureReason": "CONFIG" }] },
        "usedBy": [{ "name": "entry" }],
    }))
    .unwrap()
}

fn ssl_cert(kind: &str, status: &str, expires: &str) -> SslCertificate {
    serde_json::from_value(json!({
        "name": "old-cert",
        "selfLink": format!("{COMPUTE}/global/sslCertificates/old-cert"),
        "type": kind,
        "expireTime": expires,
        "managed": { "status": status, "domains": ["old.example.com"], "domainStatus": { "old.example.com": status } },
    }))
    .unwrap()
}

#[test]
fn a_managed_certificate_not_active_or_expiring_soon_is_bad() {
    const FAR: &str = "2027-06-01T00:00:00Z";
    const SOON: &str = "2026-11-20T00:00:00Z"; // 14 days
    const EDGE: &str = "2026-12-06T00:00:00Z"; // exactly 30
    const PAST: &str = "2026-11-01T00:00:00Z";
    let cm = |state: &str, expires: &str| Snapshot {
        cert_manager: Some(Ok(vec![manager_cert(state, expires)])),
        ..Snapshot::default()
    };
    assert!(cert_findings(&cm("ACTIVE", FAR), NOW).is_empty());

    let found = cert_findings(&cm("FAILED", FAR), NOW);
    assert_eq!((found[0].level, found.len()), (Level::Bad, 1));
    assert_eq!(found[0].what, "managed certificate is FAILED: *.example.com: CONFIG");
    assert!(cert_findings(&cm("PROVISIONING", FAR), NOW)[0].what.contains("PROVISIONING"));

    let found = cert_findings(&cm("ACTIVE", SOON), NOW);
    assert_eq!(found[0].level, Level::Bad);
    assert!(found[0].what.starts_with("certificate expires in 14 days (2026-11-20"), "{}", found[0].what);
    assert_eq!(cert_findings(&cm("ACTIVE", EDGE), NOW).len(), 1, "within 30 days");
    assert!(cert_findings(&cm("ACTIVE", PAST), NOW)[0].what.starts_with("certificate expired 5 days ago"));
    assert_eq!(cert_findings(&cm("FAILED", SOON), NOW).len(), 2, "two things wrong, each said");

    // Compute Engine's own.
    let ssl = |kind: &str, status: &str, expires: &str| Snapshot {
        lb: Some(Ok(Lb { certificates: vec![ssl_cert(kind, status, expires)], ..Lb::default() })),
        ..Snapshot::default()
    };
    assert!(cert_findings(&ssl("MANAGED", "ACTIVE", FAR), NOW).is_empty());
    let found = cert_findings(&ssl("MANAGED", "PROVISIONING_FAILED", FAR), NOW);
    assert_eq!(found[0].what, "managed certificate is PROVISIONING_FAILED: old.example.com: PROVISIONING_FAILED");
    assert_eq!(found[0].hint.as_deref(), Some("Nothing serves it."));
    // A self-managed one has no status to be wrong, and renews by no one: its expiry still counts.
    assert!(cert_findings(&ssl("SELF_MANAGED", "", FAR), NOW).is_empty());
    assert_eq!(cert_findings(&ssl("SELF_MANAGED", "", SOON), NOW)[0].level, Level::Bad);
}

#[test]
fn a_certificate_is_in_use_when_something_points_at_it() {
    let lb = Lb {
        certificates: vec![ssl_cert("MANAGED", "ACTIVE", "2027-06-01T00:00:00Z")],
        proxies: vec![TargetProxy {
            self_link: String::new(),
            url_map: String::new(),
            ssl_certificates: vec![format!("{COMPUTE}/global/sslCertificates/old-cert")],
        }],
        ..Lb::default()
    };
    let mut unused = manager_cert("ACTIVE", "2027-06-01T00:00:00Z");
    unused.used_by.clear();
    let s = Snapshot { lb: Some(Ok(lb)), cert_manager: Some(Ok(vec![unused])), ..Snapshot::default() };
    let certs = certificates(&s, NOW);
    let of = |name: &str| certs.iter().find(|c| c.name == name).unwrap();
    assert!(of("old-cert").in_use);
    assert!(!of("wild").in_use);
    assert_eq!((of("old-cert").source, of("wild").source), ("compute", "certificate manager"));
    assert_eq!(of("wild").domains, ["*.example.com"]);
}

// ---- images ---------------------------------------------------------------------------------

fn repo(id: &str, cleanup: Cleanup, mode: &str) -> Repo {
    Repo {
        id: id.into(),
        location: "us-east1".into(),
        format: "DOCKER".into(),
        mode: mode.into(),
        size: 2_000_000_000,
        cleanup,
    }
}

#[test]
fn a_repository_that_deletes_nothing_is_a_warning() {
    let s = Snapshot {
        repos: Some(Ok(vec![
            repo("none", Cleanup::Nothing, "STANDARD_REPOSITORY"),
            repo("good", Cleanup::Deletes(vec!["old".into()]), "STANDARD_REPOSITORY"),
            repo("trial", Cleanup::DryRun(vec!["old".into()]), "STANDARD_REPOSITORY"),
            repo("keeps", Cleanup::KeepOnly(vec!["pinned".into()]), "STANDARD_REPOSITORY"),
            repo("virtual", Cleanup::Nothing, "VIRTUAL_REPOSITORY"),
            repo("unread", Cleanup::Unread(denied_error()), "STANDARD_REPOSITORY"),
        ])),
        ..Snapshot::default()
    };
    let found = image_findings(&s);
    assert!(found.iter().all(|f| f.level == Level::Warn));
    let subjects: Vec<&str> = found.iter().map(|f| f.subject.as_str()).collect();
    assert_eq!(subjects, ["none", "trial", "keeps"]);
    assert_eq!(found[0].what, "no cleanup policy, and it holds 2.0 GB");
    assert!(found[1].what.contains("dry run"));
    assert!(found[2].what.contains("only keep rules"));
    // Not read is not "no policy": it is said as something unread.
    let unread = s.unread();
    assert_eq!(unread[0].what, "the cleanup policies of unread");
    let table = s.table(Section::Images, NOW);
    assert_eq!(table.rows[5].cells[4].0, "?");
    assert_eq!(table.rows[1].cells[4].0, "old");
}

#[test]
fn what_a_cleanup_policy_does_is_read_from_the_repository() {
    let of = |policies: serde_json::Value, dry: bool| {
        let repo: artifacts::Repository =
            serde_json::from_value(json!({ "cleanupPolicies": policies, "cleanupPolicyDryRun": dry })).unwrap();
        cleanup_of(&repo)
    };
    assert!(matches!(of(json!({}), false), Cleanup::Nothing));
    assert!(matches!(of(json!({ "a": { "action": "DELETE" } }), false), Cleanup::Deletes(_)));
    assert!(matches!(of(json!({ "a": { "action": "DELETE" } }), true), Cleanup::DryRun(_)));
    assert!(matches!(of(json!({ "a": { "action": "KEEP" } }), false), Cleanup::KeepOnly(_)));
    assert!(matches!(
        of(json!({ "a": { "action": "KEEP" }, "b": { "action": "DELETE" } }), false),
        Cleanup::Deletes(ids) if ids == ["a", "b"]
    ));
}

// ---- the check's exit -----------------------------------------------------------------------

#[test]
fn a_check_exits_1_for_something_bad_2_when_it_could_not_read_0_otherwise() {
    use std::process::ExitCode;
    let unread = [Unread { what: "x".into(), error: denied_error() }];
    let (warn, bad) = (Finding::warn("a", "x"), Finding::bad("b", "y"));
    assert_eq!(check_exit(&[], &[]), ExitCode::SUCCESS);
    assert_eq!(check_exit(std::slice::from_ref(&warn), &[]), ExitCode::SUCCESS);
    assert_eq!(check_exit(std::slice::from_ref(&bad), &[]), ExitCode::FAILURE);
    assert_eq!(check_exit(&[], &unread), ExitCode::from(findings::COULD_NOT_TELL));
    assert_eq!(check_exit(std::slice::from_ref(&warn), &unread), ExitCode::from(findings::COULD_NOT_TELL));
    assert_eq!(check_exit(std::slice::from_ref(&bad), &unread), ExitCode::FAILURE, "what was seen is certain");
}

// ---- against a fake Google Cloud ------------------------------------------------------------

const QUOTAS: &str = r#"{"quotas":[
    {"metric":"BACKEND_SERVICES","usage":0,"limit":200},
    {"metric":"GLOBAL_EXTERNAL_MANAGED_BACKEND_SERVICES","usage":5,"limit":200},
    {"metric":"NETWORK_ENDPOINT_GROUPS","usage":5,"limit":1000},
    {"metric":"URL_MAPS","usage":1,"limit":100},
    {"metric":"TARGET_HTTPS_PROXIES","usage":1,"limit":100},
    {"metric":"SECURITY_POLICIES","usage":0,"limit":10}]}"#;

/// One of everything: an IAP app with its public paths split off, an app open by accident, one
/// open on purpose, leftovers, a certificate of each API and a repository with no cleanup.
pub(crate) fn platform_fake() -> MockServer {
    let fake = MockServer::start(vec![]);
    let backend_json = |name: &str, neg: &str, iap: bool| {
        json!({
            "name": name,
            "selfLink": backend_url(name),
            "loadBalancingScheme": "EXTERNAL_MANAGED",
            "protocol": "HTTPS",
            "backends": [{ "group": neg_url(neg) }],
            "iap": { "enabled": iap, "oauth2ClientId": "client", "oauth2ClientSecretSha256": "abc" },
        })
    };
    let backends = json!({ "items": { "global": { "backendServices": [
        backend_json("shop-backend", "shop-neg", true),
        backend_json("shop-public-backend", "shop-public-neg", false),
        backend_json("orders-backend", "orders-neg", true),
        backend_json("gone-backend", "gone-neg", false),
        backend_json("stale-backend", "stale-neg", false),
    ] }, "regions/us-east1": { "warning": { "code": "NO_RESULTS_ON_PAGE" } } } });
    let map = json!({
        "name": "shop-map",
        "selfLink": format!("{COMPUTE}/global/urlMaps/shop-map"),
        "defaultService": backend_url("shop-backend"),
        "hostRules": [
            { "hosts": ["shop.example.com"], "pathMatcher": "shop" },
            { "hosts": ["orders.example.com"], "pathMatcher": "orders" },
            { "hosts": ["gone.example.com"], "pathMatcher": "stale" },
        ],
        "pathMatchers": [
            { "name": "shop", "defaultService": backend_url("shop-backend"),
              "pathRules": [{ "paths": ["/api/*"], "service": backend_url("shop-public-backend") }] },
            { "name": "orders", "defaultService": backend_url("orders-backend") },
            { "name": "stale", "defaultService": backend_url("stale-backend") },
        ],
    });
    let proxy = format!("{COMPUTE}/global/targetHttpsProxies/main");
    let negs: Vec<_> = [
        ("shop-neg", "shop"),
        ("shop-public-neg", "shop"),
        ("orders-neg", "orders"),
        ("gone-neg", "gone"),
        ("stale-neg", "stale"),
    ]
    .iter()
    .map(|(n, s)| {
        json!({ "name": n, "selfLink": neg_url(n), "region": format!("{COMPUTE}/regions/us-east1"),
                "networkEndpointType": "SERVERLESS", "cloudRun": { "service": s } })
    })
    .collect();
    let run = |id: &str, ingress: &str, extra: serde_json::Value| {
        let mut s = json!({
            "name": format!("projects/my-platform-123456/locations/us-east1/services/{id}"),
            "uri": format!("https://{id}-abc123-ue.a.run.app"),
            "ingress": ingress,
        });
        if let (Some(s), Some(e)) = (s.as_object_mut(), extra.as_object()) {
            s.extend(e.clone());
        }
        s
    };
    let services = json!({ "unreachable": ["us-west8"], "services": [
        run("shop", INGRESS_LB, json!({})),
        run("orders", INGRESS_ALL, json!({})),
        run("webhooks", INGRESS_ALL, json!({})),
        run("internal-tool", INGRESS_ALL, json!({})),
        run("billing", INGRESS_ALL, json!({ "invokerIamDisabled": true })),
        run("retired", INGRESS_LB, json!({})),
        run("backoffice", INGRESS_INTERNAL, json!({})),
    ] });
    let allow_all = r#"{"bindings":[{"role":"roles/run.invoker","members":["allUsers","user:ana@example.com"]}]}"#;
    let only_iam =
        r#"{"bindings":[{"role":"roles/run.invoker","members":["serviceAccount:x@p.iam.gserviceaccount.com"]}]}"#;

    let routes: Vec<(&str, String)> = vec![
        ("services/orders:getIamPolicy", allow_all.to_string()),
        ("services/webhooks:getIamPolicy", allow_all.to_string()),
        ("services/internal-tool:getIamPolicy", only_iam.to_string()),
        ("services/billing:getIamPolicy", only_iam.to_string()),
        ("/locations/us-east1/services", services.to_string()),
        ("/locations/-/services", services.to_string()),
        ("/aggregated/backendServices", backends.to_string()),
        ("/aggregated/urlMaps", json!({ "items": { "global": { "urlMaps": [map] } } }).to_string()),
        (
            "/aggregated/targetHttpsProxies",
            json!({ "items": { "global": { "targetHttpsProxies": [
                { "selfLink": proxy, "urlMap": format!("{COMPUTE}/global/urlMaps/shop-map"),
                  "certificateMap": "//certificatemanager.googleapis.com/projects/1/locations/global/certificateMaps/m" }
            ] } } })
            .to_string(),
        ),
        ("/aggregated/targetHttpProxies", "{}".to_string()),
        ("/aggregated/forwardingRules", json!({ "items": { "global": { "forwardingRules": [{ "target": proxy }] } } }).to_string()),
        (
            "/aggregated/sslCertificates",
            json!({ "items": { "global": { "sslCertificates": [{
                "name": "old-cert", "selfLink": format!("{COMPUTE}/global/sslCertificates/old-cert"), "type": "MANAGED",
                "managed": { "status": "PROVISIONING_FAILED", "domains": ["old.example.com"],
                             "domainStatus": { "old.example.com": "FAILED_NOT_VISIBLE" } } }] } } })
            .to_string(),
        ),
        ("/global/securityPolicies", "{}".to_string()),
        ("/regions/us-east1/networkEndpointGroups", json!({ "items": negs }).to_string()),
        ("/regions/us-east1", r#"{"name":"us-east1","quotas":[{"metric":"NETWORK_ENDPOINT_GROUPS","usage":0,"limit":1000}]}"#.to_string()),
        ("/locations/-/certificates", json!({ "certificates": [{
            "name": "projects/p/locations/global/certificates/wild",
            "sanDnsnames": ["*.example.com"], "expireTime": "2030-01-01T00:00:00Z",
            "pemCertificate": "-----BEGIN CERTIFICATE-----",
            "managed": { "state": "ACTIVE", "domains": ["*.example.com"] },
            "usedBy": [{ "name": "entry" }] }] }).to_string()),
        ("/repositories/images", r#"{"name":"projects/p/locations/us-east1/repositories/images","format":"DOCKER","mode":"STANDARD_REPOSITORY","sizeBytes":"1500000000"}"#.to_string()),
        ("/locations/us-east1/repositories", r#"{"repositories":[{"name":"projects/p/locations/us-east1/repositories/images","format":"DOCKER","mode":"STANDARD_REPOSITORY","sizeBytes":"1500000000"}]}"#.to_string()),
        ("/compute/v1/projects/", QUOTAS.to_string()),
    ];
    for (pattern, body) in routes {
        fake.route(pattern, Reply::json(200, &body));
    }
    fake
}

/// Every API refuses.
pub(crate) fn denied(fake: &MockServer) {
    let refused = r#"{"error":{"code":403,"status":"PERMISSION_DENIED","message":"Permission denied"}}"#;
    fake.route("/", Reply::json(403, refused));
}

fn client(fake: &MockServer) -> std::sync::Arc<Gcp> {
    Gcp::with("my-platform-123456", "us-east1", auth::tests::fixed("t"), Some(fake.url()))
}

#[test]
fn reads_everything_and_only_reads() {
    let fake = platform_fake();
    let gcp = client(&fake);
    let s = Snapshot::load(&gcp, &["webhooks".to_string(), "retired".to_string()], Want::ALL);
    assert!(s.unread().is_empty(), "{:?}", s.unread());

    let found = findings(&s, NOW);
    let subjects: Vec<(&str, Level)> = found.iter().map(|f| (f.subject.as_str(), f.level)).collect();
    assert!(subjects.contains(&("orders", Level::Bad)), "{subjects:?}");
    assert!(subjects.contains(&("billing", Level::Bad)), "{subjects:?}");
    assert!(subjects.contains(&("old-cert", Level::Bad)), "{subjects:?}");
    assert!(subjects.contains(&("orphans", Level::Warn)), "{subjects:?}");
    assert!(subjects.contains(&("retired", Level::Warn)), "{subjects:?}");
    assert!(subjects.contains(&("images", Level::Warn)), "{subjects:?}");
    assert!(!subjects.iter().any(|(s, _)| *s == "webhooks" || *s == "internal-tool" || *s == "shop"), "{subjects:?}");
    assert_eq!(found[0].level, Level::Bad, "the worst first");

    let lb = s.lb().unwrap();
    let limits = lb.limits();
    assert_eq!(limits[0].metric, "GLOBAL_EXTERNAL_MANAGED_BACKEND_SERVICES");
    assert_eq!(limits[0].used, 5);
    assert!(limits.iter().any(|l| l.what == "Host rules in shop-map" && l.used == 3));
    assert!(limits.iter().any(|l| l.what == "Path rules in shop-map" && l.used == 1));

    let requests = fake.requests();
    assert!(requests.iter().all(|r| r.method == "GET"), "only GET: {requests:?}");
    // No request ever goes to an app: only the APIs are called.
    assert!(
        requests.iter().all(|r| ["/compute/", "/run/", "/certificatemanager/", "/artifactregistry/"]
            .iter()
            .any(|api| r.path.starts_with(api))),
        "{requests:?}"
    );
    assert!(!requests.iter().any(|r| r.path.contains("run.app") || r.path.contains("example.com")), "{requests:?}");
    // One policy per service with ingress all, and none for the rest.
    let policies: Vec<&str> =
        requests.iter().filter(|r| r.path.ends_with(":getIamPolicy")).map(|r| r.path.as_str()).collect();
    assert_eq!(policies.len(), 4, "{policies:?}");
    assert!(!policies.iter().any(|p| p.contains("/shop:") || p.contains("/retired:") || p.contains("/backoffice:")));
    // The wildcard cuts its answer at a page size and gives no token: it is asked for none, and the
    // region is read page by page.
    assert!(requests.iter().any(|r| r.path == "/run/v2/projects/my-platform-123456/locations/-/services"));
    assert!(
        requests
            .iter()
            .any(|r| r.path == "/run/v2/projects/my-platform-123456/locations/us-east1/services?pageSize=100")
    );
    assert!(
        requests
            .iter()
            .any(|r| r.path == "/run/v2/projects/my-platform-123456/locations/us-east1/services/orders:getIamPolicy")
    );
    // A repository is read to know its policies: a list leaves them out.
    assert!(
        requests.iter().any(|r| r.path == "/artifactregistry/v1/projects/p/locations/us-east1/repositories/images")
    );
}

#[test]
fn what_the_commands_and_the_mcp_server_get() {
    let fake = platform_fake();
    let gcp = client(&fake);
    let s = Snapshot::load(&gcp, &["webhooks".to_string(), "retired".to_string()], Want::ALL);
    let v = s.json("platform", Want::ALL, NOW);
    assert_eq!(v["env"], "platform");
    assert_eq!(v["limits"][0]["metric"], "GLOBAL_EXTERNAL_MANAGED_BACKEND_SERVICES");
    assert_eq!(v["limits"][0]["countedFromList"], false);
    let exposure_of = |id: &str| v["exposure"].as_array().unwrap().iter().find(|r| r["service"] == id).unwrap().clone();
    assert_eq!(exposure_of("orders")["direct"], true);
    assert_eq!(exposure_of("orders")["publicInvoker"]["member"], "allUsers");
    assert_eq!(exposure_of("orders")["iap"], "iap");
    assert_eq!(exposure_of("webhooks")["listed"], true);
    assert_eq!(exposure_of("shop")["iap"], "iap + public");
    assert_eq!(exposure_of("shop")["direct"], false);
    assert_eq!(v["exposedList"], json!(["webhooks", "retired"]));
    assert_eq!(v["certificates"][0]["name"], "old-cert", "the one that needs attention first");
    assert_eq!(v["images"][0]["sizeBytes"], 1_500_000_000u64);
    assert_eq!(v["orphans"].as_array().unwrap().len(), 4);
    assert_eq!(v["unread"], json!([]));
    // People named in a policy, a client's secret hash and a certificate's body stay out.
    let text = v.to_string();
    for private in ["ana@example.com", "oauth2Client", "BEGIN CERTIFICATE", "serviceAccount:"] {
        assert!(!text.contains(private), "{private} in {text}");
    }
    // Only what is asked for.
    let only_images = Want::only(Section::Images);
    let s = Snapshot::load(&gcp, &[], only_images);
    let v = s.json("platform", only_images, NOW);
    assert!(v.get("limits").is_none() && v.get("exposure").is_none());
    assert_eq!(v["findings"].as_array().unwrap().len(), 1);
    assert!(s.lb.is_none() && s.services.is_none());
}

#[test]
fn a_refused_read_is_said_and_leaves_the_rest_standing() {
    let fake = MockServer::start(vec![]);
    let refused = r#"{"error":{"code":403,"status":"PERMISSION_DENIED","message":"Permission denied"}}"#;
    fake.route("/certificatemanager/", Reply::json(403, refused));
    fake.route("/artifactregistry/", Reply::json(200, "{}"));
    fake.route("/services", Reply::json(200, "{}"));
    fake.route("/compute/", Reply::json(403, r#"{"error":{"code":403,"status":"PERMISSION_DENIED","message":"Compute Engine API has not been used in project p before or it is disabled."}}"#));
    let s = Snapshot::load(&client(&fake), &[], Want::ALL);
    let unread = s.unread();
    assert_eq!(unread.len(), 1, "an API that is off is not a read that failed: {unread:?}");
    assert_eq!(unread[0].what, "Certificate Manager's certificates");
    assert!(s.notes()[0].contains("Compute Engine API is off"), "{:?}", s.notes());
    assert!(findings(&s, NOW).is_empty());
    assert_eq!(check_exit(&findings(&s, NOW), &unread), std::process::ExitCode::from(findings::COULD_NOT_TELL));
}

#[test]
fn without_a_region_every_one_is_read() {
    let fake = MockServer::start(vec![]);
    fake.route(
        "/regions/us-east1/networkEndpointGroups",
        Reply::json(200, r#"{"items":[{"name":"n","selfLink":"x","region":"https://x/regions/us-east1"}]}"#),
    );
    fake.route("/regions/europe-west1/networkEndpointGroups", Reply::json(200, "{}"));
    fake.route(
        "/regions",
        Reply::json(200, r#"{"items":[{"name":"us-east1","quotas":[]},{"name":"europe-west1","quotas":[]}]}"#),
    );
    fake.route("/repositories", Reply::json(200, r#"{"repositories":[]}"#));
    fake.route("/artifactregistry/v1/projects/p/locations", Reply::json(200, r#"{"locations":[{"locationId":"us"}]}"#));
    fake.route("/compute/", Reply::json(200, "{}"));
    let gcp = Gcp::with("p", "", auth::tests::fixed("t"), Some(fake.url()));
    let only = Want { limits: true, orphans: false, exposure: false, certificates: false, images: true };
    let s = Snapshot::load(&gcp, &[], only);
    assert!(s.unread().is_empty(), "{:?}", s.unread());
    assert_eq!(s.lb().unwrap().regions.len(), 2);
    assert_eq!(s.lb().unwrap().negs.len(), 1);
    let paths: Vec<String> = fake.requests().iter().map(|r| r.path.clone()).collect();
    assert!(paths.contains(&"/compute/compute/v1/projects/p/regions/europe-west1/networkEndpointGroups".to_string()));
    assert!(paths.contains(&"/artifactregistry/v1/projects/p/locations/us/repositories?pageSize=1000".to_string()));
}

#[test]
fn a_list_cut_short_by_the_wildcard_never_makes_a_service_look_deleted() {
    let shop = r#"{"name":"projects/p/locations/us-east1/services/shop"}"#;
    let orders = r#"{"name":"projects/p/locations/us-east1/services/orders"}"#;
    let abroad = r#"{"name":"projects/p/locations/europe-west1/services/abroad"}"#;
    let fake = MockServer::start(vec![]);
    fake.route(
        "/locations/-/services",
        Reply::json(200, &format!(r#"{{"services":[{shop},{abroad}],"unreachable":["us-west8"]}}"#)),
    );
    fake.route("/locations/europe-west1/services", Reply::json(200, &format!(r#"{{"services":[{abroad}]}}"#)));
    // The second page is what the wildcard left out.
    fake.route("pageToken=more", Reply::json(200, &format!(r#"{{"services":[{orders}]}}"#)));
    fake.route(
        "/locations/us-east1/services",
        Reply::json(200, &format!(r#"{{"services":[{shop}],"nextPageToken":"more"}}"#)),
    );
    let gcp = Gcp::with("p", "us-east1", auth::tests::fixed("t"), Some(fake.url()));
    let read = read_services(&gcp).unwrap();
    assert_eq!(read.list.iter().map(Service::id).collect::<Vec<_>>(), ["abroad", "orders", "shop"]);
    assert_eq!(read.unreachable, ["us-west8"]);
    // A region that cannot be read is a failed read, not an empty one.
    let fake = MockServer::start(vec![]);
    fake.route("/locations/-/services", Reply::json(200, "{}"));
    fake.route(
        "/locations/us-east1/services",
        Reply::json(503, r#"{"error":{"code":503,"status":"UNAVAILABLE","message":"down"}}"#),
    );
    let gcp = Gcp::with("p", "us-east1", auth::tests::fixed("t"), Some(fake.url()));
    assert!(read_services(&gcp).is_err());
}

#[test]
fn every_section_has_rows_for_the_terminal() {
    let fake = platform_fake();
    let s = Snapshot::load(&client(&fake), &["webhooks".to_string()], Want::ALL);
    for section in Section::ALL {
        let table = s.table(section, NOW);
        assert!(!table.rows.is_empty(), "{section:?}");
        assert!(table.rows.iter().all(|r| r.cells.len() == table.cols.len()), "{section:?}");
        assert!(table.rows.iter().all(|r| r.console.starts_with("https://console.cloud.google.com/")), "{section:?}");
    }
    assert_eq!(wanted(Want::ALL).len(), 5);
    // An overview leaves out the services the internet cannot get to.
    let exposure = s.table(Section::Exposure, NOW);
    let overview: Vec<&str> = exposure.rows.iter().filter(|r| r.overview).map(|r| r.name.as_str()).collect();
    assert!(overview.contains(&"orders") && overview.contains(&"webhooks"), "{overview:?}");
    assert!(!overview.contains(&"shop"), "{overview:?}");
}
