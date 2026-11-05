//! Costs: what the environment's project costs, from the billing export in BigQuery (see
//! `crate::costs`). Overview (this month against the same complete days of the last, the
//! forecast, budgets, credits), services with their SKUs, apps (Cloud Run services) with what
//! they share, and spikes.
//!
//! What was read shows at once. BigQuery bills the bytes a query scans, so a read is dry-run
//! and priced first: opening the module reads again by itself when what was read is older than
//! `defaults.costs.autoRefreshHours` and the estimate is under `autoRefreshMaxUsd`; otherwise,
//! and for `r` over that limit, it asks.

use std::collections::HashSet;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use crate::config::CostDefaults;
use crate::costs::day::Day;
use crate::costs::report::{self, AppRow, ServiceRow, SharedRow, View as Period, change, money};
use crate::costs::spikes::{self, Rule, Spike};
use crate::costs::{self, CostError, Kind, Prepared, Snapshot, ledger};
use crate::tui::chart::sparkline;
use crate::tui::form::{Field, Form, FormEvent};
use crate::tui::module::{Ctx, Handled, Level, Module, Payload, View};
use crate::tui::{theme, widgets};
use crate::util::{ellipsize, fmt_ago, fmt_utc, now_secs};

/// Complete days the Spikes tab goes back.
const SPIKE_DAYS: i64 = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Overview,
    Services,
    Apps,
    Spikes,
}

const TABS: [Tab; 4] = [Tab::Overview, Tab::Services, Tab::Apps, Tab::Spikes];

enum Msg {
    Estimated { result: Result<Prepared, CostError>, auto: bool },
    Read { result: Result<Snapshot, CostError>, kept: Result<(), String> },
}

enum Phase {
    Idle,
    Estimating,
    /// Over the limit: the estimate, waiting for a yes.
    Asking(Box<Prepared>, Form),
    /// Reading; what it costs.
    Reading(String),
}

/// A row of the Services tab: a service, or one of its SKUs once it is opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Service(usize),
    Sku(usize, usize),
}

pub struct Costs {
    env: String,
    costs: CostDefaults,
    snapshot: Option<Snapshot>,
    phase: Phase,
    error: Option<CostError>,
    /// Billing not configured: what to set.
    unconfigured: Option<String>,
    /// The auto-refresh decision was made for this visit of the environment.
    checked: bool,
    ledger: ledger::Month,
    tab: Tab,
    period: Option<Period>,
    services: Vec<ServiceRow>,
    opened: HashSet<String>,
    rows: Vec<Row>,
    services_state: ListState,
    apps: Vec<AppRow>,
    shared: Vec<SharedRow>,
    apps_state: ListState,
    /// The last complete days, newest first, with their spikes (an error for a day that
    /// cannot be checked).
    spikes: Vec<(Day, Result<Vec<Spike>, String>)>,
    scroll: u16,
    list_area: Rect,
}

impl Costs {
    pub fn new() -> Costs {
        Costs {
            env: String::new(),
            costs: CostDefaults::default(),
            snapshot: None,
            phase: Phase::Idle,
            error: None,
            unconfigured: None,
            checked: false,
            ledger: ledger::Month::default(),
            tab: Tab::Overview,
            period: None,
            services: Vec::new(),
            opened: HashSet::new(),
            rows: Vec::new(),
            services_state: ListState::default(),
            apps: Vec::new(),
            shared: Vec::new(),
            apps_state: ListState::default(),
            spikes: Vec::new(),
            scroll: 0,
            list_area: Rect::default(),
        }
    }

    fn rule(&self) -> Rule {
        Rule::of(&self.costs)
    }

    /// Recomputes every view from the snapshot.
    fn rebuild(&mut self) {
        let rule = self.rule();
        self.period = None;
        self.services.clear();
        self.apps.clear();
        self.shared.clear();
        self.spikes.clear();
        if let Some(s) = &self.snapshot {
            self.period = report::month_view(s, Day::of(s.read_at).month_start());
            if let Some(p) = &self.period {
                self.services = report::services(s, p);
                (self.apps, self.shared) = report::apps(s, p, &rule);
            }
            if let Some(last) = s.last_complete_day() {
                self.spikes =
                    (0..SPIKE_DAYS).map(|i| last.plus(-i)).map(|d| (d, report::spikes(s, d, &rule))).collect();
            }
        }
        self.rebuild_rows();
        let n = self.apps.len();
        self.apps_state.select(if n == 0 { None } else { Some(self.apps_state.selected().unwrap_or(0).min(n - 1)) });
    }

    fn rebuild_rows(&mut self) {
        self.rows.clear();
        let month = self.snapshot.as_ref().map(|s| Day::of(s.read_at).month_start());
        for (i, svc) in self.services.iter().enumerate() {
            self.rows.push(Row::Service(i));
            if let (true, Some(s), Some(m)) = (self.opened.contains(&svc.service), &self.snapshot, month) {
                let skus = report::skus(s, m);
                self.rows.extend(
                    skus.iter().enumerate().filter(|(_, k)| k.service == svc.service).map(|(j, _)| Row::Sku(i, j)),
                );
            }
        }
        let n = self.rows.len();
        self.services_state.select(if n == 0 {
            None
        } else {
            Some(self.services_state.selected().unwrap_or(0).min(n - 1))
        });
    }

    /// The spikes of the last complete day.
    fn last_spikes(&self) -> &[Spike] {
        match self.spikes.first() {
            Some((_, Ok(found))) => found,
            _ => &[],
        }
    }

    // ---- reading -------------------------------------------------------------------------

    fn busy(&self) -> bool {
        matches!(self.phase, Phase::Estimating | Phase::Reading(_))
    }

    fn start_estimate(&mut self, ctx: &mut Ctx, auto: bool) {
        if self.busy() {
            return ctx.toast(Level::Info, "Already reading the costs");
        }
        if let Some(why) = &self.unconfigured {
            return ctx.toast(Level::Warn, why.clone());
        }
        self.phase = Phase::Estimating;
        self.error = None;
        let env = ctx.env();
        let now = now_secs();
        ctx.spawn(move || Msg::Estimated {
            result: costs::prepare(&env.gcp, &env.name, &env.config, Kind::Current, now),
            auto,
        });
    }

    fn start_read(&mut self, ctx: &mut Ctx, prepared: Prepared) {
        let estimate = prepared.describe(self.costs.bigquery_usd_per_tib);
        self.phase = Phase::Reading(estimate);
        let env = ctx.env();
        let costs = self.costs.clone();
        ctx.spawn(move || {
            let result = costs::run(&env.gcp, prepared, now_secs());
            let kept = match &result {
                Ok(s) => costs::store(s, &costs).map_err(|e| e.to_string()),
                Err(_) => Ok(()),
            };
            Msg::Read { result, kept }
        });
    }

    fn ask_form(&self, p: &Prepared) -> Form {
        let price = costs::usd(self.ledger.bytes, self.costs.bigquery_usd_per_tib);
        let mut form = Form::new(
            format!("Read the costs of {}", self.env),
            vec![
                Field::fixed("Estimate", p.describe(self.costs.bigquery_usd_per_tib)),
                Field::fixed(
                    "This month",
                    format!(
                        "{} queries, {} ({})",
                        self.ledger.queries,
                        report::bytes(self.ledger.bytes),
                        report::usd_small(price)
                    ),
                ),
            ],
        )
        .submit_label("read")
        .enter_submits();
        form.note = Some(format!(
            "BigQuery bills the bytes each query scans. This read is over defaults.costs.autoRefreshMaxUsd (US$ {:.2}), so it asks first. What is read is kept for this module, gcli costs and the MCP server.",
            self.costs.auto_refresh_max_usd
        ));
        form
    }

    /// A new reading: notifies the spikes it has that the one before did not.
    fn read(&mut self, s: Snapshot, ctx: &mut Ctx) {
        let rule = self.rule();
        let fresh = appeared(self.snapshot.as_ref(), &s, &rule);
        let first_read = self.snapshot.is_none();
        let billed = report::usd_small(costs::usd(s.bytes_billed, self.costs.bigquery_usd_per_tib));
        ctx.toast(
            Level::Success,
            format!("Read the costs of {}: {} billed ({billed})", self.env, report::bytes(s.bytes_billed)),
        );
        let currency = s.currency.clone();
        self.snapshot = Some(s);
        self.ledger = ledger::month(&Day::of(now_secs()).month_key());
        self.rebuild();
        let Some(first) = fresh.first() else { return };
        let body = if fresh.len() == 1 {
            format!(
                "{} cost {} on {} ({})",
                first.name,
                money(first.cost, &currency),
                first.day.short(),
                first.reason()
            )
        } else {
            let names: Vec<&str> = fresh.iter().map(|x| x.name.as_str()).collect();
            format!("{} on {}: {}", fresh.len(), first.day.short(), names.join(", "))
        };
        let title = format!("Cost spike in {}", self.env);
        // A first reading has nothing to compare with: shown, not pushed to the desktop.
        if first_read {
            ctx.toast(Level::Warn, format!("{title} — {body}"))
        } else {
            ctx.notify(Level::Warn, &title, &body)
        }
    }

    // ---- moving ----------------------------------------------------------------------------

    fn move_by(&mut self, delta: i64) {
        let (state, len) = match self.tab {
            Tab::Services => (&mut self.services_state, self.rows.len()),
            Tab::Apps => (&mut self.apps_state, self.apps.len()),
            Tab::Overview | Tab::Spikes => {
                self.scroll = (i64::from(self.scroll) + delta).clamp(0, u16::MAX as i64) as u16;
                return;
            }
        };
        if len > 0 {
            let i = state.selected().unwrap_or(0) as i64 + delta;
            state.select(Some(i.clamp(0, len as i64 - 1) as usize));
        }
    }

    fn toggle(&mut self) {
        let Some(Row::Service(i) | Row::Sku(i, _)) =
            self.services_state.selected().and_then(|i| self.rows.get(i)).copied()
        else {
            return;
        };
        let name = self.services[i].service.clone();
        if !self.opened.remove(&name) {
            self.opened.insert(name);
        }
        self.rebuild_rows();
        if let Some(at) = self.rows.iter().position(|r| *r == Row::Service(i)) {
            self.services_state.select(Some(at));
        }
    }

    fn page(&self) -> i64 {
        i64::from(self.list_area.height.saturating_sub(4).max(1))
    }

    // ---- drawing ---------------------------------------------------------------------------

    fn render_header(&self, f: &mut Frame, area: Rect, view: &View) {
        let mut first = vec![Span::styled(format!(" {}", self.env), theme::bold())];
        match &self.snapshot {
            Some(s) => {
                first.push(Span::styled(format!(" · {} · read {}", s.project, fmt_ago(s.read_at)), theme::muted()));
                if s.data_through > 0.0 {
                    first.push(Span::styled(format!(" · data through {}", fmt_utc(s.data_through)), theme::muted()));
                }
                first.push(Span::styled(" · UTC days", theme::muted()));
            }
            None => first.push(Span::styled(" · not read yet", theme::muted())),
        }
        let status = match &self.phase {
            Phase::Estimating => Some(format!("{} estimating the read (dry run, free)…", theme::spinner(view.tick))),
            Phase::Reading(estimate) => {
                Some(format!("{} reading the billing export: {estimate}…", theme::spinner(view.tick)))
            }
            Phase::Asking(..) | Phase::Idle => None,
        };
        let tabs: Vec<Span> = TABS
            .iter()
            .flat_map(|t| {
                let label = match t {
                    Tab::Overview => "Overview".to_string(),
                    Tab::Services => "By service".to_string(),
                    Tab::Apps => "By app".to_string(),
                    Tab::Spikes => match self.last_spikes().len() {
                        0 => "Spikes".to_string(),
                        n => format!("Spikes {n}"),
                    },
                };
                let style = if *t == self.tab {
                    theme::selected()
                } else if *t == Tab::Spikes && !self.last_spikes().is_empty() {
                    Style::new().fg(theme::ERR)
                } else {
                    theme::muted()
                };
                [Span::raw(" "), Span::styled(format!(" {label} "), style)]
            })
            .collect();
        let mut second = tabs;
        match status {
            Some(text) => second.push(Span::styled(format!("   {text}"), theme::accent())),
            None => {
                let price = report::usd_small(costs::usd(self.ledger.bytes, self.costs.bigquery_usd_per_tib));
                second.push(Span::styled(
                    format!(
                        "   BigQuery this month: {} queries, {} ({price})",
                        self.ledger.queries,
                        report::bytes(self.ledger.bytes)
                    ),
                    theme::muted(),
                ));
            }
        }
        f.render_widget(Paragraph::new(vec![Line::from(first), Line::from(second)]), area);
    }

    fn empty(&self, f: &mut Frame, area: Rect) {
        let mut lines = vec![Line::raw("")];
        if let Some(why) = &self.unconfigured {
            lines.push(Line::styled(why.clone(), Style::new().fg(theme::WARN)));
        } else {
            if let Some(e) = &self.error {
                lines.push(Line::styled(format!("Could not read the costs: {e}"), Style::new().fg(theme::ERR)));
                lines.push(Line::raw(""));
            }
            lines.push(Line::raw(format!("The costs of {} have not been read yet.", self.env)));
            lines.push(Line::raw(""));
            lines.push(Line::from(vec![
                Span::styled("r", theme::key()),
                Span::raw(" reads the billing export: BigQuery bills the bytes scanned, so the estimate is shown first. What is read is kept."),
            ]));
        }
        let block = widgets::panel(" Costs ", true);
        f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).block(block), area);
    }

    fn overview_lines(&self, s: &Snapshot) -> Vec<Line<'static>> {
        let c = s.currency.clone();
        let month = Day::of(s.read_at).month_start();
        let mut lines = Vec::new();
        if let Some(e) = &self.error {
            lines.push(Line::styled(format!("Could not read the costs again: {e}"), Style::new().fg(theme::ERR)));
            lines.push(Line::raw(""));
        }
        let Some(o) = report::overview(s, month) else { return lines };
        let kw = 22;
        let totals = |t: report::Totals| {
            vec![
                Span::styled("gross ", theme::muted()),
                Span::styled(format!("{:>10}", money(t.gross, &c)), theme::bold()),
                Span::styled("   net ", theme::muted()),
                Span::raw(format!("{:>10}", money(t.net, &c))),
            ]
        };
        lines.push(widgets::section(&month.month_name()));
        if let Some(r) = o.complete {
            let mut spans = vec![Span::styled(format!("{:<kw$}", format!("{} (complete)", r.label())), theme::muted())];
            spans.extend(totals(o.complete_total));
            if let (Some(ch), Some(p)) = (change(o.complete_total.gross, o.previous_total.gross), o.previous) {
                let color =
                    if o.complete_total.gross > o.previous_total.gross * 1.05 { theme::WARN } else { theme::OK };
                spans.push(Span::styled(format!("   {ch}"), Style::new().fg(color)));
                spans.push(Span::styled(
                    format!(" vs {} ({})", p.label(), money(o.previous_total.gross, &c)),
                    theme::muted(),
                ));
            }
            lines.push(Line::from(spans));
        }
        for p in &o.partial {
            lines.push(Line::from(vec![
                Span::styled(format!("{:<kw$}", format!("{} (partial)", p.day.short())), theme::muted()),
                Span::styled(
                    format!("gross {:>10} so far: the export runs 4 to 12 hours behind", money(p.gross, &c)),
                    theme::muted(),
                ),
            ]));
        }
        let mut mtd = vec![Span::styled(format!("{:<kw$}", "Month to date"), theme::muted())];
        mtd.extend(totals(o.total));
        lines.push(Line::from(mtd));
        if let (Some(fc), Some(r)) = (o.forecast, o.complete) {
            let mut spans = vec![Span::styled(format!("{:<kw$}", "Forecast"), theme::muted())];
            spans.extend(totals(fc));
            spans.push(Span::styled(format!("   straight line from {} complete days", r.days()), theme::muted()));
            lines.push(Line::from(spans));
        }
        if let Some(p) = o.previous_month {
            let mut spans = vec![Span::styled(format!("{:<kw$}", month.previous_month().month_name()), theme::muted())];
            spans.extend(totals(p));
            lines.push(Line::from(spans));
        }
        if let Some(a) = o.account {
            let mut text = format!("this month gross {} · net {}", money(a.gross, &c), money(a.net, &c));
            if let Some(p) = o.account_previous {
                text.push_str(&format!(" · last month gross {} · net {}", money(p.gross, &c), money(p.net, &c)));
            }
            lines.push(Line::from(vec![
                Span::styled(format!("{:<kw$}", "Billing account"), theme::muted()),
                Span::raw(text),
            ]));
        }

        let budgets = report::budgets(s);
        if !budgets.is_empty() {
            lines.push(Line::raw(""));
            lines.push(widgets::section("Budgets"));
            let w = budgets.iter().map(|b| b.name.chars().count()).max().unwrap_or(0).clamp(10, 40);
            for b in &budgets {
                let currency = if b.currency.is_empty() { c.as_str() } else { b.currency.as_str() };
                let ratio = b.ratio().unwrap_or(0.0);
                let color = if ratio >= 1.0 {
                    theme::ERR
                } else if b.thresholds.iter().any(|t| t.crossed) {
                    theme::WARN
                } else {
                    theme::OK
                };
                let mut spans = vec![
                    Span::styled(if b.covers_project { "● " } else { "  " }, theme::accent()),
                    Span::raw(format!("{:<w$} ", ellipsize(&b.name, w))),
                ];
                spans.extend(widgets::progress_bar(ratio, 12, color));
                spans.push(Span::raw(format!(
                    " {:>9} of {:<9} {:>4.0} %",
                    money(b.spent, currency),
                    b.amount.map_or("?".into(), |a| money(a, currency)),
                    (ratio * 100.0).max(0.0)
                )));
                let mut rest: Vec<String> = Vec::new();
                if let Some(fc) = b.forecast {
                    rest.push(format!("forecast {}", money(fc, currency)));
                }
                for t in &b.thresholds {
                    rest.push(format!(
                        "{}{:.0} %{}",
                        if t.forecast { "forecast " } else { "" },
                        t.percent * 100.0,
                        if t.crossed { " ✗" } else { "" }
                    ));
                }
                spans.push(Span::styled(format!("  {}", rest.join(" · ")), theme::muted()));
                lines.push(Line::from(spans));
                for n in &b.notes {
                    lines.push(Line::styled(format!("    {n}"), theme::muted()));
                }
            }
            lines.push(Line::styled("● covers this project", theme::muted()));
        }
        if !o.credits.is_empty() {
            lines.push(Line::raw(""));
            lines.push(widgets::section(&format!("Credits in {} (this project)", month.month_name())));
            for cr in o.credits.iter().take(12) {
                lines.push(Line::from(vec![
                    Span::raw(format!("{:<10} {:>10}  ", cr.kind, money(cr.amount, &c))),
                    Span::styled(ellipsize(&cr.name, 100), theme::muted()),
                ]));
            }
        }
        for n in &s.notes {
            lines.push(Line::raw(""));
            lines.push(Line::styled(format!("note: {n}"), theme::muted()));
        }
        lines
    }

    fn render_overview(&self, f: &mut Frame, area: Rect, s: &Snapshot) {
        let lines = self.overview_lines(s);
        let block = widgets::panel(" Overview ", true);
        f.render_widget(
            Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((self.scroll, 0)).block(block),
            area,
        );
    }

    fn render_services(&mut self, f: &mut Frame, area: Rect) {
        self.list_area = area;
        let Some(s) = &self.snapshot else { return };
        let c = s.currency.as_str();
        let month = Day::of(s.read_at).month_start();
        let skus = report::skus(s, month);
        let width = area.width.saturating_sub(4) as usize;
        let spark = width >= 100;
        let fixed = 10 + 2 + 10 + 2 + 16 + 2 + if spark { report::SPARK_DAYS as usize + 2 } else { 0 };
        // As wide as the longest name shown (SKUs are indented by two), within the room left.
        let longest = self
            .rows
            .iter()
            .map(|row| match *row {
                Row::Service(i) => self.services[i].service.chars().count(),
                Row::Sku(_, j) => skus[j].sku.chars().count() + 2,
            })
            .max()
            .unwrap_or(0);
        let name_w = width.saturating_sub(fixed + 2).min(longest.max(36)).max(12);
        let compare = self.period.and_then(|p| p.before).map_or(String::new(), |b| format!("vs {}", b.label()));
        let header = format!(
            "  {:<name_w$}{:>10}  {:>10}  {:<16}  {}",
            "service",
            "gross",
            "net",
            compare,
            if spark { format!("last {} days", report::SPARK_DAYS) } else { String::new() }
        );
        let items: Vec<ListItem> = self
            .rows
            .iter()
            .map(|row| match *row {
                Row::Service(i) => {
                    let r = &self.services[i];
                    let marker = if self.opened.contains(&r.service) { "▾ " } else { "▸ " };
                    let ch = r.before.and_then(|b| change(r.now, b)).unwrap_or_default();
                    let mut spans = vec![
                        Span::styled(marker, theme::muted()),
                        Span::raw(format!("{:<name_w$}", ellipsize(&r.service, name_w))),
                        Span::styled(format!("{:>10}", money(r.total.gross, c)), theme::bold()),
                        Span::raw(format!("  {:>10}  {:<16}  ", money(r.total.net, c), ch)),
                    ];
                    if spark {
                        let values: Vec<Option<f64>> = r.daily.iter().map(|v| Some(*v)).collect();
                        spans.push(Span::styled(format!("{:<30}  ", sparkline(&values)), theme::accent()));
                    }
                    spans.push(Span::styled(if r.by_app { "per app" } else { "shared" }, theme::muted()));
                    ListItem::new(Line::from(spans))
                }
                Row::Sku(_, j) => {
                    let k = skus[j];
                    let (amount, unit, rate) = report::sku_usage(k, c);
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            format!("    {:<w$}", ellipsize(&k.sku, name_w.saturating_sub(2)), w = name_w - 2),
                            theme::muted(),
                        ),
                        Span::raw(format!("{:>10}", money(k.cost, c))),
                        Span::styled(format!("  {amount:>8} {unit}  ·  {rate}"), theme::muted()),
                    ]))
                }
            })
            .collect();
        let title = format!(" {} by service · Enter: SKUs and their rates ", month.month_name());
        let block = widgets::panel(title, true);
        let inner = block.inner(area);
        f.render_widget(block, area);
        let [head, list] = inner.layout(&Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]));
        f.render_widget(Paragraph::new(Line::styled(header, theme::muted().bold())), head);
        f.render_stateful_widget(List::new(items).highlight_style(theme::selected()), list, &mut self.services_state);
    }

    fn render_apps(&mut self, f: &mut Frame, area: Rect) {
        let Some(s) = &self.snapshot else { return };
        let c = s.currency.as_str();
        let shared_h = (self.shared.len() as u16 + 3).min(area.height / 2);
        let [top, bottom] = area.layout(&Layout::vertical([Constraint::Fill(1), Constraint::Length(shared_h)]));
        self.list_area = top;
        let width = top.width.saturating_sub(4) as usize;
        let spark = width >= 96;
        let last = s.last_complete_day().map_or("last day".into(), Day::short);
        let fixed = 10 + 2 + 10 + 2 + 10 + 2 + if spark { report::SPARK_DAYS as usize + 2 } else { 0 } + 8;
        let name_w = width.saturating_sub(fixed).clamp(12, 48);
        let header = format!(
            "{:<name_w$}  {:>10}  {:>10}  {:>10}  {}",
            "app (Cloud Run service)",
            "month",
            last,
            "median 14d",
            if spark { format!("last {} days", report::SPARK_DAYS) } else { String::new() }
        );
        let items: Vec<ListItem> = self
            .apps
            .iter()
            .map(|a| {
                let mut spans = vec![
                    Span::raw(format!("{:<name_w$}  ", ellipsize(&a.app, name_w))),
                    Span::styled(format!("{:>10}", money(a.total, c)), theme::bold()),
                    Span::styled(
                        format!("  {:>10}", money(a.last_day, c)),
                        if a.spike { Style::new().fg(theme::ERR).bold() } else { Style::new() },
                    ),
                    Span::styled(format!("  {:>10}  ", a.median14.map_or("—".into(), |m| money(m, c))), theme::muted()),
                ];
                if spark {
                    let values: Vec<Option<f64>> = a.daily.iter().map(|v| Some(*v)).collect();
                    spans.push(Span::styled(format!("{:<30}  ", sparkline(&values)), theme::accent()));
                }
                if a.spike {
                    spans.push(Span::styled("✗ spike", Style::new().fg(theme::ERR)));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();
        let total: f64 = self.apps.iter().map(|a| a.total).sum();
        let block = widgets::panel(format!(" Apps · {} · {} this month ", self.apps.len(), money(total, c)), true);
        let inner = block.inner(top);
        f.render_widget(block, top);
        let [head, list] = inner.layout(&Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]));
        f.render_widget(Paragraph::new(Line::styled(header, theme::muted().bold())), head);
        f.render_stateful_widget(List::new(items).highlight_style(theme::selected()), list, &mut self.apps_state);

        let shared_total: f64 = self.shared.iter().map(|r| r.total).sum();
        let lines: Vec<Line> = self
            .shared
            .iter()
            .map(|r| {
                Line::from(vec![
                    Span::raw(format!("{:<name_w$}  ", ellipsize(&r.service, name_w))),
                    Span::raw(format!("{:>10}", money(r.total, c))),
                ])
            })
            .collect();
        let title = format!(
            " Shared · {} this month · no app can be told from these: shown whole, never spread ",
            money(shared_total, c)
        );
        f.render_widget(Paragraph::new(Text::from(lines)).block(widgets::panel(title, false)), bottom);
    }

    fn render_spikes(&self, f: &mut Frame, area: Rect) {
        let c = self.snapshot.as_ref().map_or("USD", |s| s.currency.as_str());
        let mut lines = Vec::new();
        for (day, found) in &self.spikes {
            match found {
                Ok(found) if found.is_empty() => lines.push(Line::from(vec![
                    Span::styled(format!("{day}  "), theme::bold()),
                    Span::styled("no spike", Style::new().fg(theme::OK)),
                ])),
                Ok(found) => {
                    lines.push(Line::styled(
                        format!("{day}  {} spike{}", found.len(), if found.len() == 1 { "" } else { "s" }),
                        Style::new().fg(theme::ERR).bold(),
                    ));
                    for x in found {
                        let kind = match x.kind {
                            spikes::Kind::App => "app",
                            spikes::Kind::Shared => "shared",
                        };
                        lines.push(Line::from(vec![
                            Span::styled("  ✗ ", Style::new().fg(theme::ERR)),
                            Span::raw(format!("{:<36} ", ellipsize(&x.name, 36))),
                            Span::styled(format!("{kind:<7}"), theme::muted()),
                            Span::styled(format!("{:>10}  ", money(x.cost, c)), theme::bold()),
                            Span::raw(x.reason()),
                            Span::styled(
                                format!("  ({} of the 14 days before had a cost)", x.history_days),
                                theme::muted(),
                            ),
                        ]));
                    }
                }
                Err(e) => lines.push(Line::from(vec![
                    Span::styled(format!("{day}  "), theme::bold()),
                    Span::styled(e.clone(), theme::muted()),
                ])),
            }
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            format!(
                "Rule: {} (defaults.costs in the config). UTC days; today, still partial, is never checked.",
                self.rule().describe()
            ),
            theme::muted(),
        ));
        let block = widgets::panel(format!(" Spikes · the last {SPIKE_DAYS} complete days "), true);
        f.render_widget(
            Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((self.scroll, 0)).block(block),
            area,
        );
    }
}

/// The spikes of `new`'s last complete day that `old`'s reading did not show (all of them
/// when there was none).
fn appeared(old: Option<&Snapshot>, new: &Snapshot, rule: &Rule) -> Vec<Spike> {
    let on_last =
        |s: &Snapshot| s.last_complete_day().and_then(|d| report::spikes(s, d, rule).ok()).unwrap_or_default();
    let before: Vec<(String, Day)> =
        old.map(on_last).unwrap_or_default().into_iter().map(|x| (x.name, x.day)).collect();
    on_last(new).into_iter().filter(|x| !before.contains(&(x.name.clone(), x.day))).collect()
}

impl Module for Costs {
    fn title(&self) -> &'static str {
        "Costs"
    }

    fn description(&self) -> &'static str {
        "Billing export (BigQuery) · month to date, budgets, services and SKUs, apps, spikes"
    }

    fn reset(&mut self, ctx: &mut Ctx) {
        let env = ctx.env();
        *self = Costs::new();
        self.env = env.name.clone();
        self.costs = ctx.config().defaults.costs.clone();
        self.unconfigured = costs::billing_of(&env.name, &env.config).err().map(|e| e.message);
        self.snapshot = costs::load(&env.name, Kind::Current);
        self.ledger = ledger::month(&Day::of(now_secs()).month_key());
        self.rebuild();
    }

    fn activate(&mut self, ctx: &mut Ctx) {
        if self.checked || self.unconfigured.is_some() {
            return;
        }
        self.checked = true;
        let stale = self.snapshot.as_ref().is_none_or(|s| s.stale(now_secs(), self.costs.auto_refresh_hours));
        if stale {
            self.start_estimate(ctx, true);
        }
    }

    fn key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        if let Phase::Asking(_, form) = &mut self.phase {
            match form.handle(key) {
                FormEvent::Submit => {
                    if let Phase::Asking(p, _) = std::mem::replace(&mut self.phase, Phase::Idle) {
                        self.start_read(ctx, *p);
                    }
                }
                FormEvent::Cancel => self.phase = Phase::Idle,
                FormEvent::Pending => {}
            }
            return Handled::Yes;
        }
        let i = TABS.iter().position(|t| *t == self.tab).unwrap_or(0);
        match key.code {
            KeyCode::Right | KeyCode::Char('l') => {
                self.tab = TABS[(i + 1) % TABS.len()];
                self.scroll = 0;
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.tab = TABS[(i + TABS.len() - 1) % TABS.len()];
                self.scroll = 0;
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::PageUp => self.move_by(-self.page()),
            KeyCode::PageDown => self.move_by(self.page()),
            KeyCode::Home => self.move_by(-100_000),
            KeyCode::End => self.move_by(100_000),
            KeyCode::Enter if self.tab == Tab::Services => self.toggle(),
            KeyCode::Char('r') => self.start_estimate(ctx, false),
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn mouse(&mut self, event: MouseEvent, _ctx: &mut Ctx) -> Handled {
        if matches!(self.phase, Phase::Asking(..)) {
            return Handled::No;
        }
        match event.kind {
            MouseEventKind::ScrollDown => self.move_by(1),
            MouseEventKind::ScrollUp => self.move_by(-1),
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn task(&mut self, payload: Payload, ctx: &mut Ctx) {
        let Ok(msg) = payload.downcast::<Msg>() else { return };
        match *msg {
            Msg::Estimated { result: Err(e), .. } | Msg::Read { result: Err(e), .. } => {
                self.phase = Phase::Idle;
                let text = if e.login {
                    "Google Cloud credentials expired — sign in again (L)".to_string()
                } else {
                    e.message.clone()
                };
                ctx.toast(Level::Error, format!("Could not read the costs: {text}"));
                self.error = Some(e);
            }
            Msg::Estimated { result: Ok(p), auto } => {
                let price = costs::usd(p.billed, self.costs.bigquery_usd_per_tib);
                if price <= self.costs.auto_refresh_max_usd {
                    if auto {
                        ctx.toast(
                            Level::Info,
                            format!("Reading the costs again ({})", p.describe(self.costs.bigquery_usd_per_tib)),
                        );
                    }
                    self.start_read(ctx, p);
                } else {
                    let form = self.ask_form(&p);
                    self.phase = Phase::Asking(Box::new(p), form);
                }
            }
            Msg::Read { result: Ok(s), kept } => {
                self.phase = Phase::Idle;
                self.error = None;
                if let Err(e) = kept {
                    ctx.toast(Level::Warn, format!("Could not keep what was read: {e}"));
                }
                self.read(s, ctx);
            }
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let [head, body] = area.layout(&Layout::vertical([Constraint::Length(2), Constraint::Fill(1)]));
        self.render_header(f, head, view);
        match (&self.snapshot, self.tab) {
            (None, _) => self.empty(f, body),
            (Some(s), Tab::Overview) => self.render_overview(f, body, s),
            (Some(_), Tab::Services) => self.render_services(f, body),
            (Some(_), Tab::Apps) => self.render_apps(f, body),
            (Some(_), Tab::Spikes) => self.render_spikes(f, body),
        }
        if let Phase::Asking(_, form) = &self.phase {
            form.render(f, area, theme::WARN);
        }
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if matches!(self.phase, Phase::Asking(..)) {
            return vec![("Enter", "read"), ("Esc", "cancel")];
        }
        let mut hints = vec![("←→", "tab")];
        match self.tab {
            Tab::Services => hints.extend([("↑↓", "service"), ("Enter", "SKUs")]),
            Tab::Apps => hints.push(("↑↓", "app")),
            Tab::Overview | Tab::Spikes => hints.push(("↑↓", "scroll")),
        }
        hints.push(("r", "read again"));
        hints
    }

    fn badge(&self) -> Option<(String, Color)> {
        let n = self.last_spikes().len();
        (n > 0).then(|| (format!("{n} spike{}", if n == 1 { "" } else { "s" }), theme::ERR))
    }

    fn animating(&self) -> bool {
        self.busy()
    }

    fn capturing(&self) -> bool {
        matches!(self.phase, Phase::Asking(..))
    }

    /// Another module sent the user here for an app: its row in By app.
    fn open(&mut self, target: &str, _ctx: &mut Ctx) {
        if let Some(i) = self.apps.iter().position(|a| a.app == target) {
            self.tab = Tab::Apps;
            self.apps_state.select(Some(i));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::costs::report::tests::snapshot;
    use crate::costs::{SkuCost, sql::CLOUD_RUN};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn screen(m: &mut Costs, width: u16, height: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(width, height)).unwrap();
        t.draw(|f| {
            let area = f.area();
            m.render(f, area, &View { tick: 0, now: 0.0 });
        })
        .unwrap();
        let buf = t.backend().buffer().clone();
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn module() -> Costs {
        let mut m = Costs::new();
        m.env = "platform".into();
        let mut s = snapshot();
        s.skus = vec![SkuCost {
            month: "2026-11".into(),
            service: "Cloud Run".into(),
            sku: "Services CPU (Request-based billing)".into(),
            unit: "seconds".into(),
            cost: 12.0,
            amount: 500_000.0,
        }];
        m.snapshot = Some(s);
        m.rebuild();
        m
    }

    #[test]
    fn every_tab_draws_what_was_read() {
        let mut m = module();
        assert_eq!(m.badge(), Some(("1 spike".to_string(), theme::ERR)));
        let text = screen(&mut m, 160, 44);
        for expected in ["Nov 1–4 (complete)", "Nov 5 (partial)", "Forecast", "platform gross", "PROMOTION", "Spikes 1"]
        {
            assert!(text.contains(expected), "{expected} in\n{text}");
        }
        m.tab = Tab::Services;
        let text = screen(&mut m, 160, 30);
        assert!(text.contains("Cloud SQL") && text.contains("shared") && text.contains("per app"), "{text}");
        // Enter on Cloud Run (second by cost) opens its SKUs, with their rate.
        m.services_state
            .select(m.rows.iter().position(|r| matches!(r, Row::Service(i) if m.services[*i].service_id == CLOUD_RUN)));
        m.toggle();
        let text = screen(&mut m, 160, 30);
        assert!(text.contains("Services CPU (Request-based billing)") && text.contains("per second"), "{text}");
        m.tab = Tab::Apps;
        let text = screen(&mut m, 160, 30);
        assert!(text.contains("orders") && text.contains("✗ spike") && text.contains("never spread"), "{text}");
        m.tab = Tab::Spikes;
        let text = screen(&mut m, 160, 30);
        assert!(text.contains("2026-11-04  1 spike") && text.contains("3.0× its 14-day median"), "{text}");
        // Narrow: no sparklines, nothing breaks.
        m.tab = Tab::Apps;
        screen(&mut m, 60, 20);
    }

    #[test]
    fn without_a_reading_it_says_how_to_read() {
        let mut m = Costs::new();
        m.env = "platform".into();
        let text = screen(&mut m, 120, 20);
        assert!(text.contains("have not been read yet") && text.contains("estimate is shown first"), "{text}");
        m.unconfigured = Some("set environments.platform.billing.exportTable".into());
        assert!(screen(&mut m, 120, 20).contains("billing.exportTable"));
        assert_eq!(m.badge(), None);
    }

    #[test]
    fn only_new_spikes_are_announced() {
        let rule = Rule { ratio: 2.0, min_usd: 1.0, new_min_usd: 2.0 };
        let new = snapshot();
        assert_eq!(appeared(None, &new, &rule).len(), 1, "a first reading shows its spikes");
        assert!(appeared(Some(&new), &new, &rule).is_empty(), "already seen");
        let mut old = new.clone();
        old.daily.retain(|d| d.day != Day::parse("2026-11-04").unwrap());
        old.data_through = Day::parse("2026-11-04").unwrap().start();
        let found = appeared(Some(&old), &new, &rule);
        assert_eq!(found.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), ["orders"]);
    }
}
