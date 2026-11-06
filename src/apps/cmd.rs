//! The Apps commands: `gcli apps` (the list, `--held` as a check), `gcli app <app>` (one app,
//! `--report` for an AI agent) and the two changes, `gcli apps restart` and `gcli apps min`.

use std::io::BufRead;
use std::process::ExitCode;

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use super::health::{HELD_EXPLAINED, Range};
use super::report::versions_summary;
use super::{Column, Detail, Listing, Row, Sort, Tone, cells, columns, costs, report, sort};
use crate::cli::{confirm, gcp_for, interactive, paint, print_json};
use crate::config::Config;
use crate::gcp::run;
use crate::tui::chart::{fmt_count, fmt_ms, sparkline};
use crate::util::{ellipsize, fmt_ago, fmt_duration, fmt_utc, now_secs, program};

pub(crate) fn tone(text: &str, t: Tone) -> String {
    match t {
        Tone::Plain => text.to_string(),
        Tone::Muted => paint(text, "90"),
        Tone::Warn => paint(text, "33"),
        Tone::Bad => paint(text, "31"),
    }
}

pub(crate) fn warn(text: &str) {
    eprintln!("{}", paint(&format!("! {}", crate::util::sanitize(text)), "33"));
}

/// A table of `cols`: each column as wide as its widest cell (at most 40), the last one as long
/// as it needs.
pub(crate) fn print_table(cols: &[Column], table: Vec<Vec<(String, Tone)>>) {
    print_table_wide(cols, table, 40);
}

/// [`print_table`] with columns of up to `widest` characters.
pub(crate) fn print_table_wide(cols: &[Column], table: Vec<Vec<(String, Tone)>>, widest: usize) {
    let last = cols.len() - 1;
    let widths: Vec<usize> = (0..cols.len())
        .map(|i| {
            table.iter().map(|r| r[i].0.chars().count()).chain([cols[i].head.len()]).max().unwrap_or(0).min(widest)
        })
        .collect();
    let line = |items: Vec<(String, Tone)>| {
        let parts: Vec<String> = items
            .into_iter()
            .enumerate()
            .map(|(i, (text, t))| {
                if i == last {
                    return tone(&text, t);
                }
                let text = ellipsize(&text, widths[i]);
                let padded = if cols[i].right {
                    format!("{text:>w$}", w = widths[i])
                } else {
                    format!("{text:<w$}", w = widths[i])
                };
                tone(&padded, t)
            })
            .collect();
        parts.join("  ").trim_end().to_string()
    };
    println!("{}", paint(&line(cols.iter().map(|c| (c.head.to_string(), Tone::Plain)).collect()), "90;1"));
    for row in table {
        println!("{}", line(row));
    }
}

/// `gcli apps [env] [--sort …] [--held] [--json]`. With `--held`, only the apps holding
/// requests at their timeout, and exit 1 when there are any.
pub fn list(config: &Config, env_name: &str, by: Sort, held_only: bool, json: bool) -> Result<ExitCode> {
    let env = config.env(env_name)?;
    let gcp = gcp_for(config, env_name)?;
    let listing = Listing::load(&gcp, env_name, now_secs())?;
    if let Err(e) = &listing.health {
        if held_only {
            bail!("cannot tell which apps hold requests: the metrics could not be read: {}", e.short());
        }
        warn(&format!("health columns empty: Cloud Monitoring could not be read: {}", e.short()));
    }
    if let Err(e) = &listing.costs {
        warn(&format!("cost columns hidden: {e}"));
    }
    let mut rows = listing.rows();
    sort(&mut rows, by, |r| *r);
    if held_only {
        rows.retain(Row::held);
    }
    let code = if held_only && !rows.is_empty() { ExitCode::FAILURE } else { ExitCode::SUCCESS };
    if json {
        print_json(&Value::Array(rows.iter().map(Row::json).collect()));
        return Ok(code);
    }
    let costs = listing.costs.as_ref().ok().and_then(Option::as_ref);
    if held_only && rows.is_empty() {
        println!("{} No app holds requests at its timeout ({env_name}, last 24 h).", paint("✓", "32"));
        return Ok(code);
    }
    print_table(&columns(costs.is_some()), rows.iter().map(|r| cells(r, costs.is_some())).collect());
    let held = rows.iter().filter(|r| r.held()).count();
    let mut footer = vec![format!("{} app{}", rows.len(), if rows.len() == 1 { "" } else { "s" })];
    if held > 0 && !held_only {
        footer.push(format!("{held} held"));
    }
    footer.push(format!("{env_name} · {}", env.summary()));
    footer.push(format!("health over the last 24 h · sorted by {}", by.label()));
    footer.push(match costs {
        Some(c) => c.freshness(),
        None => "no cost data yet".into(),
    });
    println!("\n{}", paint(&footer.join(" · "), "90"));
    if held > 0 {
        println!("{}", paint(&format!("held: {HELD_EXPLAINED}"), "90"));
    }
    Ok(code)
}

/// `gcli app <app> [env] [--json | --report]`: configuration, health, revisions, secrets.
pub fn detail(config: &Config, env_name: &str, app: &str, json: bool, markdown: bool) -> Result<ExitCode> {
    let env = config.env(env_name)?;
    let gcp = gcp_for(config, env_name)?;
    let now = now_secs();
    let detail = Detail::load(&gcp, env.apps, app, Range::Day, now)?;
    let costs = costs::read(env_name).unwrap_or_else(|e| {
        warn(&e);
        None
    });
    if json {
        print_json(&detail.json(&env.project, costs.as_ref().and_then(|c| c.app(app))));
    } else if markdown {
        print!("{}", report::markdown(env_name, &env.project, &detail, now));
    } else {
        print_detail(&detail, &env.project, env_name);
    }
    Ok(ExitCode::SUCCESS)
}

fn print_detail(d: &Detail, project: &str, env_name: &str) {
    let s = &d.service;
    let kv = |k: &str, v: String| println!("  {}  {v}", paint(&format!("{k:<9}"), "90"));
    println!(
        "{}  {}  {}",
        paint(s.id(), "1"),
        paint(&format!("{env_name} · {}", s.region()), "90"),
        paint(&s.uri, "36")
    );
    let created = d.revision_created().map(|t| format!(" · {} ({})", fmt_ago(t), fmt_utc(t))).unwrap_or_default();
    let traffic = s.traffic_split().unwrap_or_else(|| "100% latest".into());
    kv("Serving", format!("{}{created} · traffic {traffic}", s.revision()));
    if s.latest_not_ready() {
        kv("Newest", paint(&format!("{} is not ready", run::last_segment(&s.latest_created_revision)), "31"));
    }
    match &d.last_deploy {
        Ok(Some(f)) => kv("Deploy", paint(&f.text(), "31")),
        Ok(None) => {}
        Err(e) => kv("Deploy", paint(&format!("! could not read the builds: {}", e.short()), "33")),
    }
    let max = s.max_instances().map_or("default".into(), |m| m.to_string());
    kv(
        "Scaling",
        format!("min {} (service {} · revision {}) · max {max}", s.min_instances(), s.service_min(), s.revision_min()),
    );
    kv(
        "CPU",
        if s.cpu_always_on() {
            paint("always on (instance-based billing)", "33")
        } else {
            "only during requests".into()
        },
    );
    let concurrency = s.template.max_instance_request_concurrency.map_or("default".into(), |c| c.to_string());
    kv(
        "Timeout",
        format!("{} · concurrency {concurrency} · ingress {}", fmt_ms(s.timeout_secs() * 1000.0), s.ingress_label()),
    );

    match &d.charts {
        Ok(c) => {
            let h = c.summary();
            println!("\n{}", paint(&format!("HEALTH · last {}", c.range.label()), "36;1"));
            let spark = |points: &[(f64, f64)]| {
                let n = (c.range.secs() / c.range.period() as f64) as usize;
                paint(&sparkline(&crate::tui::chart::columns(points, c.window, n, crate::tui::chart::Agg::Sum)), "36")
            };
            let pct = |class: &str| h.percent(class).map_or("—".into(), |p| format!("{p:.2}%"));
            kv(
                "Requests",
                format!(
                    "{} · 4xx {} · 5xx {}   {}",
                    fmt_count(h.requests),
                    pct("4xx"),
                    pct("5xx"),
                    spark(&c.requests())
                ),
            );
            let ms = |q: f64| h.percentile(q).map_or("—".into(), fmt_ms);
            kv("Latency", format!("p50 {} · p95 {} · p99 {}", ms(0.5), ms(0.95), ms(0.99)));
            let max_instances = c.instances().iter().map(|p| p.1).fold(0.0, f64::max);
            kv(
                "Billed",
                format!(
                    "{} of instance time · up to {} instances   {}",
                    fmt_duration(h.billed_secs),
                    fmt_count(max_instances),
                    spark(&c.billed)
                ),
            );
            let timeout = s.timeout_secs();
            let near = h.near_timeout(timeout);
            if h.held(timeout) {
                kv("Held", paint(&format!("yes · about {} requests at the timeout", fmt_count(near.round())), "31"));
                println!("             {}", paint(HELD_EXPLAINED, "90"));
            } else if near >= 1.0 {
                kv("Held", format!("no (p99) · about {} requests ran ≥ 90% of the timeout", fmt_count(near.round())));
            } else {
                kv("Held", "no".into());
            }
        }
        Err(e) => println!("\n{}", paint(&format!("! metrics: {}", e.short()), "33")),
    }

    println!("\n{}", paint("REVISIONS", "36;1"));
    match &d.revisions {
        Ok(revs) => {
            for r in revs.iter().take(5) {
                let when = r.created().map(|t| format!("{:>8}", fmt_ago(t))).unwrap_or_default();
                let restart = if r.labels.contains_key(run::RESTART_LABEL) {
                    paint(" ↻ gcli restart", "36")
                } else {
                    String::new()
                };
                let serving = if r.id() == s.revision() { paint(" ● serving", "32") } else { String::new() };
                println!(
                    "  {}  {}  {}  {}{serving}{restart}",
                    s.short(r.id()),
                    paint(&when, "90"),
                    r.image_short(),
                    paint(&r.creator, "90")
                );
                let names = r.plain_env_names();
                if !names.is_empty() {
                    println!("      {} {}", paint("vars", "90"), names.join(" "));
                }
                for b in r.secret_refs() {
                    println!("      {} {} ← {}:{}", paint("secret", "90"), b.target, b.secret_id(), b.version);
                }
            }
            if revs.len() > 5 {
                println!("  {}", paint(&format!("… {} more in --json", revs.len() - 5), "90"));
            }
        }
        Err(e) => println!("  {}", paint(&format!("! {}", e.short()), "33")),
    }

    match &d.secrets {
        Ok(_) => {
            let states = d.secret_states(project);
            let bound = states.iter().filter(|s| s.bound()).count();
            println!(
                "\n{}",
                paint(
                    &format!(
                        "SECRETS · {} · {bound} bound · {} not bound (values never read)",
                        states.len(),
                        states.len() - bound
                    ),
                    "36;1"
                )
            );
            let w = states.iter().map(|s| s.secret.key.chars().count()).max().unwrap_or(0).min(48);
            for st in &states {
                let read_as = if st.bound() {
                    st.bindings.iter().map(|b| format!("{} ← {}", b.target, b.version)).collect::<Vec<_>>().join(", ")
                } else {
                    paint("not bound", "90")
                };
                let versions = match &st.secret.versions {
                    Ok(vs) => versions_summary(vs),
                    Err(e) => format!("unreadable: {}", e.short()),
                };
                println!("  {:<w$}  {read_as}", ellipsize(&st.secret.key, w));
                println!("  {:<w$}  {}", "", paint(&versions, "90"));
                for f in &st.findings {
                    println!(
                        "  {:<w$}  {}",
                        "",
                        paint(&format!("! {}", f.text()), if f.serious() { "31" } else { "33" })
                    );
                }
            }
        }
        Err(e) => println!("\n{}", paint(&format!("! secrets: {}", e.short()), "33")),
    }
}

/// Asks before a change: `--yes` goes ahead; in a terminal, production asks for the app's
/// name to be typed and other environments for a yes; anything else refuses.
fn confirmed(production: bool, app: &str, yes: bool, json: bool, question: &str) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if json {
        bail!("with --json, confirm with --yes (the question would mix with the output)");
    }
    if !interactive() {
        bail!("this changes {app}: run it in a terminal to confirm, or pass --yes");
    }
    if !production {
        return Ok(confirm(question));
    }
    print!("{} Type the app's name ('{app}') to confirm: ", paint("production:", "31;1"));
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(answer.trim() == app)
}

/// `gcli apps restart <app> [env] [--yes]`: a new revision with the same configuration, so
/// instances start again and read `latest` secrets.
pub fn restart(config: &Config, env_name: &str, app: &str, yes: bool, json: bool) -> Result<ExitCode> {
    let env = config.env(env_name)?;
    let gcp = gcp_for(config, env_name)?;
    let service = run::find_service(&gcp, app)?;
    if !json {
        println!(
            "Restarting {app} ({env_name}) makes a new revision with the same configuration (label {}). Traffic that\n\
             follows the latest revision moves to it once it is ready, and its instances start fresh, re-reading secrets\n\
             bound as `latest`.",
            run::RESTART_LABEL
        );
        if service.pinned_percent() > 0 {
            warn(&format!(
                "{}% of the traffic is pinned to older revisions and stays there: those instances are not restarted",
                service.pinned_percent()
            ));
        }
    }
    if !confirmed(env.is_production(env_name), app, yes, json, &format!("Restart {app}?"))? {
        println!("Not restarted.");
        return Ok(ExitCode::FAILURE);
    }
    if !json {
        println!("{}", paint("Waiting for the new revision (a minute or two)…", "90"));
    }
    let revision = run::restart(&gcp, &service.name, now_secs() as u64)?;
    if json {
        print_json(&json!({ "app": app, "revision": revision }));
    } else {
        println!("{} Restarted {app}: {revision}", paint("✓", "32"));
    }
    Ok(ExitCode::SUCCESS)
}

/// The largest minimum Cloud Run accepts.
const MAX_MIN_INSTANCES: u32 = 1000;

/// Checks a minimum against the service's maximum.
pub fn check_min(service: &run::Service, n: u32) -> Result<(), String> {
    if n > MAX_MIN_INSTANCES {
        return Err(format!("at most {MAX_MIN_INSTANCES}"));
    }
    match service.max_instances() {
        Some(max) if n > max => Err(format!("{} allows at most {max} instances", service.id())),
        _ => Ok(()),
    }
}

/// `gcli apps min <app> <n> [env] [--yes]`: the service-level minimum (no new revision).
pub fn min(config: &Config, env_name: &str, app: &str, n: u32, yes: bool, json: bool) -> Result<ExitCode> {
    let env = config.env(env_name)?;
    let gcp = gcp_for(config, env_name)?;
    let service = run::find_service(&gcp, app)?;
    check_min(&service, n).map_err(|e| anyhow!(e))?;
    if !json {
        println!(
            "{app} ({env_name}): service-level minimum {} → {n}, applied at once without a new revision. Warm instances\n\
             are billed even without requests.",
            service.service_min()
        );
        if service.revision_min() > n {
            warn(&format!(
                "the revision's own minimum ({}) is higher and still applies: change it with a deploy",
                service.revision_min()
            ));
        }
    }
    if !confirmed(env.is_production(env_name), app, yes, json, &format!("Set the minimum of {app} to {n}?"))? {
        println!("Not changed.");
        return Ok(ExitCode::FAILURE);
    }
    let updated = run::set_min_instances(&gcp, &service.name, n)?;
    let effective = updated.service_min().max(service.revision_min());
    if json {
        print_json(&json!({ "app": app, "serviceMin": updated.service_min(), "effectiveMin": effective }));
    } else {
        println!(
            "{} {app}: minimum {} (effective {effective}). See it with: {} app {app} {env_name}",
            paint("✓", "32"),
            updated.service_min(),
            program()
        );
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimums_stay_within_the_maximum() {
        let s: run::Service = serde_json::from_str(
            r#"{"name":"projects/p/locations/r/services/shop","template":{"scaling":{"maxInstanceCount":4}}}"#,
        )
        .unwrap();
        assert!(check_min(&s, 4).is_ok());
        assert_eq!(check_min(&s, 5).unwrap_err(), "shop allows at most 4 instances");
        assert!(check_min(&run::Service::default(), 1001).is_err());
    }
}
