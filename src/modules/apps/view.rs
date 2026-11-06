//! Drawing the Apps module: the list (columns that give way on narrow terminals) with the
//! selected app below it, an app's detail in three tabs, and the change dialogs.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, Paragraph, Wrap};

use super::{Apps, Open, TABS, Tab};
use crate::apps::health::{Charts, HELD_EXPLAINED, HELD_SHARE};
use crate::apps::report::versions_summary;
use crate::apps::{Tone, cells, columns, secrets};
use crate::gcp::run::{self, Service};
use crate::modules::common::{LoadState, filter_bar, fit, spinner_line, tone_style as style};
use crate::tui::chart::{Agg, Chart, fmt_count, fmt_ms};
use crate::tui::module::View;
use crate::tui::theme;
use crate::tui::widgets;
use crate::util::{ellipsize, fmt_ago, fmt_duration, fmt_utc};

impl Apps {
    pub(super) fn render_list(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let [list, preview] = area.layout(&Layout::vertical([Constraint::Fill(1), Constraint::Length(8)]));
        self.list_area = list;
        let [bar, list_area] = list.layout(&Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]));
        let (bar_line, cursor) = filter_bar(&self.filter, self.filtering, "type / to search apps");
        f.render_widget(Paragraph::new(bar_line), bar);
        if let Some(col) = cursor {
            f.set_cursor_position((bar.x + col, bar.y));
        }
        let mut title = vec![Span::raw(" ")];
        match &self.load {
            LoadState::Loading => {
                title.push(Span::raw(format!("{} Apps · {}", theme::spinner(view.tick), self.services.len())))
            }
            _ if self.visible.len() < self.services.len() => {
                title.push(Span::raw(format!("Apps · {} of {}", self.visible.len(), self.services.len())))
            }
            _ => title.push(Span::raw(format!("Apps · {}", self.services.len()))),
        }
        if let LoadState::Failed(e) = &self.load
            && !self.services.is_empty()
        {
            title.push(Span::styled(format!(" · ✗ {}", e.short()), Style::new().fg(theme::ERR)));
        }
        title.push(Span::styled(format!(" · sorted by {}", self.sort.label()), theme::muted()));
        match &self.health {
            None => title.push(Span::styled(format!(" · {} last 24 h", theme::spinner(view.tick)), theme::muted())),
            Some(Err(e)) => {
                title.push(Span::styled(format!(" · no metrics: {}", e.short()), Style::new().fg(theme::WARN)))
            }
            Some(Ok(_)) => title.push(Span::styled(" · last 24 h", theme::muted())),
        }
        match &self.costs {
            Ok(Some(c)) => title.push(Span::styled(format!(" · {}", c.freshness()), theme::muted())),
            Ok(None) => {}
            Err(e) => title.push(Span::styled(format!(" · costs: {e}"), Style::new().fg(theme::WARN))),
        }
        title.push(Span::raw(" "));
        let title = Line::from(title);

        if self.visible.is_empty() {
            let text = match &self.load {
                LoadState::Failed(e) => Line::styled(e.short(), Style::new().fg(theme::ERR)),
                LoadState::Loading | LoadState::Idle => Line::styled("Loading apps…", theme::muted()),
                LoadState::Loaded if !self.services.is_empty() => Line::styled("No app matches", theme::muted()),
                LoadState::Loaded => {
                    let region = if self.region.is_empty() { "any region" } else { self.region.as_str() };
                    Line::styled(format!("No Cloud Run services in {} ({region})", self.project), theme::muted())
                }
            };
            f.render_widget(widgets::placeholder(text).block(widgets::panel(title, true)), list_area);
            self.render_preview(f, preview);
            return;
        }

        let with_costs = self.cost_file().is_some();
        let cols = columns(with_costs);
        let table: Vec<Vec<(String, Tone)>> =
            self.visible.iter().map(|&i| cells(&self.row(&self.services[i]), with_costs)).collect();
        let width = list_area.width.saturating_sub(4) as usize;
        let mut widths: Vec<usize> = (0..cols.len())
            .map(|i| table.iter().map(|r| r[i].0.chars().count()).chain([cols[i].head.len()]).max().unwrap_or(0))
            .collect();
        widths[0] = widths[0].min(32);
        let drops: Vec<u8> = cols.iter().map(|c| c.drop).collect();
        let keep = fit(&widths, &drops, width);
        let last = cols.len() - 1;
        let fixed: usize = (0..last).filter(|&i| keep[i]).map(|i| widths[i] + 2).sum();
        widths[last] = width.saturating_sub(fixed);
        if fixed > width {
            // Still too narrow: the name gives way.
            widths[0] = widths[0].saturating_sub(fixed - width).max(8);
        }
        let render_cells = |row: Vec<(String, Tone)>, head: bool| -> Line<'static> {
            let mut spans = Vec::new();
            for (i, (text, tone)) in row.into_iter().enumerate().filter(|(i, _)| keep[*i]) {
                let w = widths[i];
                let text = ellipsize(&text, w);
                let cell = if cols[i].right { format!("{text:>w$}") } else { format!("{text:<w$}") };
                let s = if head { theme::muted().bold() } else { style(tone) };
                spans.push(Span::styled(cell, s));
                if i != last {
                    spans.push(Span::raw("  "));
                }
            }
            Line::from(spans)
        };
        let header = render_cells(cols.iter().map(|c| (c.head.to_string(), Tone::Plain)).collect(), true);
        let items: Vec<ListItem> = table.into_iter().map(|row| ListItem::new(render_cells(row, false))).collect();
        let block = widgets::panel(title, true);
        let inner = block.inner(list_area);
        f.render_widget(block, list_area);
        let [head, rows] = inner.layout(&Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]));
        f.render_widget(Paragraph::new(header), head);
        f.render_stateful_widget(List::new(items).highlight_style(theme::selected()), rows, &mut self.list_state);
        self.render_preview(f, preview);
    }

    /// The selected app in a few lines, under the list.
    fn render_preview(&self, f: &mut Frame, area: Rect) {
        let Some(s) = self.selected() else {
            f.render_widget(
                widgets::placeholder(Line::styled("Select an app", theme::muted()))
                    .block(widgets::panel(" App ", false)),
                area,
            );
            return;
        };
        let row = self.row(s);
        let kw = 9;
        let mut lines = config_lines(s, kw);
        lines.truncate(3);
        if let Some(h) = row.health {
            let ms = |q: f64| h.percentile(q).map_or("—".into(), fmt_ms);
            let pct = |c: &str| h.percent(c).map_or("—".into(), |p| format!("{p:.1}%"));
            let text = format!(
                "{} requests · 4xx {} · 5xx {} · p50 {} · p99 {} · billed {}",
                fmt_count(h.requests),
                pct("4xx"),
                pct("5xx"),
                ms(0.5),
                ms(0.99),
                fmt_duration(h.billed_secs)
            );
            lines.push(widgets::kv("Last 24 h", text, kw));
        }
        if row.held() {
            lines.push(widgets::kv("Held", Span::styled(HELD_EXPLAINED, Style::new().fg(theme::ERR)), kw));
        } else if let Some(c) = row.cost {
            let spike = if c.spike { Span::styled(" · spike", Style::new().fg(theme::ERR)) } else { Span::raw("") };
            lines.push(Line::from(vec![
                Span::styled(format!("{:<kw$} ", "Cost"), theme::muted()),
                Span::raw(format!(
                    "${:.2} this month · ${:.2} yesterday · ${:.2} median day",
                    c.month_to_date, c.yesterday, c.median14
                )),
                spike,
            ]));
        }
        let block = widgets::panel(
            Line::from(vec![Span::raw(format!(" {} ", s.id())).bold(), Span::styled("Enter opens ", theme::muted())]),
            false,
        );
        f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).block(block), area);
    }

    pub(super) fn render_detail(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let Some(open) = &self.open else { return };
        let Some(s) = self.service(&open.id) else {
            f.render_widget(widgets::placeholder(spinner_line(view.tick, "reading the app…")), area);
            return;
        };
        let row = self.row(s);
        let mut block = widgets::panel(Line::from(format!(" {} ", s.id())).bold(), true);
        if row.held() {
            block = block.title_top(Line::styled(" ● held ", Style::new().fg(theme::ERR).bold()).right_aligned());
        } else if open.loading() {
            block = block.title_top(spinner_line(view.tick, "reading ").right_aligned());
        }
        let inner = block.inner(area);
        f.render_widget(block, area);
        let [head, tabs, body] =
            inner.layout(&Layout::vertical([Constraint::Length(1), Constraint::Length(2), Constraint::Fill(1)]));
        let about = format!("{} · {} · {} · {}", self.env_name, s.region(), s.revision(), s.uri);
        f.render_widget(Paragraph::new(Line::styled(ellipsize(&about, head.width as usize), theme::muted())), head);
        f.render_widget(Paragraph::new(tabs_line(open, self.findings().iter().filter(|f| f.serious()).count())), tabs);
        let width = body.width.saturating_sub(1) as usize;
        let lines = match open.tab {
            Tab::Health => self.health_lines(open, s, width, view),
            Tab::Revisions => revision_lines(open, s, view),
            Tab::Secrets => self.secret_lines(open, s, view),
        };
        // Scrolling stops at the end.
        let height = widgets::wrapped_height(&lines, body.width) as usize;
        let max = height.saturating_sub(body.height as usize) as u16;
        let scroll = open.scroll.min(max);
        self.detail_area = body;
        if let Some(o) = &mut self.open {
            o.scroll = scroll;
        }
        f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((scroll, 0)), body);
    }

    fn health_lines(&self, open: &Open, s: &Service, width: usize, view: &View) -> Vec<Line<'static>> {
        let kw = 11;
        let mut lines = config_lines(s, kw);
        match &open.deploy {
            Some(Ok(Some(f))) => {
                let text = format!("{} · b shows its builds", f.text());
                lines.push(widgets::kv("Deploy", Span::styled(text, Style::new().fg(theme::ERR)), kw));
            }
            Some(Err(e)) => {
                let text = format!("could not read the builds: {}", e.short());
                lines.push(widgets::kv("Deploy", Span::styled(text, Style::new().fg(theme::WARN)), kw));
            }
            _ => {}
        }
        if let Some(c) = self.row(s).cost {
            let spike = if c.spike { " · spike" } else { "" };
            lines.push(widgets::kv(
                "Cost",
                Span::styled(
                    format!(
                        "${:.2} this month · ${:.2} yesterday · ${:.2} median day{spike}",
                        c.month_to_date, c.yesterday, c.median14
                    ),
                    if c.spike { Style::new().fg(theme::ERR) } else { Style::new() },
                ),
                kw,
            ));
        }
        lines.push(Line::raw(""));
        let mut ranges = vec![widgets::section("Health").spans.remove(0), Span::raw("   ")];
        for r in [crate::apps::health::Range::Day, crate::apps::health::Range::Hours6] {
            let st = if r == open.range { theme::accent().bold() } else { theme::muted() };
            ranges.push(Span::styled(format!("{} ", r.label()), st));
        }
        ranges.push(Span::styled("  w changes", theme::muted()));
        lines.push(Line::from(ranges));
        match open.charts.get(&open.range) {
            None => lines.push(spinner_line(view.tick, "reading the metrics…")),
            Some(Err(e)) => lines.push(Line::styled(e.short(), Style::new().fg(theme::ERR))),
            Some(Ok(c)) => lines.extend(chart_lines(c, s, width)),
        }
        lines
    }

    fn secret_lines(&self, open: &Open, s: &Service, view: &View) -> Vec<Line<'static>> {
        let list = match &open.secrets {
            None => return vec![spinner_line(view.tick, "reading the secrets…")],
            Some(Err(e)) => return vec![Line::styled(e.short(), Style::new().fg(theme::ERR))],
            Some(Ok(list)) => list,
        };
        if list.is_empty() {
            let text = match self.convention.secret_prefix(s.id()) {
                Some(prefix) => format!("No {prefix}* secrets, and the service reads none."),
                None => "The service reads no secrets.".into(),
            };
            return vec![Line::styled(text, theme::muted())];
        }
        let created = crate::apps::revision_created(s, open.revisions.as_ref().and_then(|r| r.as_deref().ok()));
        let states = secrets::states(&self.project, list, &s.secret_refs(), created);
        let bound = states.iter().filter(|st| st.bound()).count();
        let mut lines = vec![Line::styled(
            format!(
                "{} secrets · {bound} bound · {} not bound · values are never read",
                states.len(),
                states.len() - bound
            ),
            theme::muted(),
        )];
        if states.iter().flat_map(|st| &st.findings).any(|f| matches!(f, secrets::Finding::NotLoaded { .. })) {
            lines.push(Line::styled(
                "Cloud Run resolves a variable bound to `latest` when an instance starts: R restarts the app so its instances read the new version.",
                Style::new().fg(theme::WARN),
            ));
        }
        lines.push(Line::raw(""));
        for st in &states {
            let mut head = vec![Span::styled(st.secret.key.clone(), theme::bold())];
            if !st.secret.own {
                head.push(Span::styled("  (read, not named after the app)", theme::muted()));
            }
            lines.push(Line::from(head));
            if st.bound() {
                for b in &st.bindings {
                    let how = if b.mounted { "file " } else { "" };
                    lines.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(format!("{how}{}", b.target), theme::accent()),
                        Span::styled(" ← ", theme::muted()),
                        Span::raw(b.version.clone()),
                    ]));
                }
            } else {
                lines.push(Line::styled("  not bound to the service", theme::muted()));
            }
            match &st.secret.versions {
                Ok(vs) => {
                    lines.push(Line::styled(format!("  {}", versions_summary(vs)), theme::muted()));
                    for v in vs.iter().take(4) {
                        let when =
                            v.created().map(|t| format!("created {} ({})", fmt_utc(t), fmt_ago(t))).unwrap_or_default();
                        let destroyed = crate::util::parse_rfc3339(&v.destroy_time)
                            .map(|t| format!(" · destroyed {}", fmt_utc(t)))
                            .unwrap_or_default();
                        let st_style = if v.enabled() { Style::new().fg(theme::OK) } else { theme::muted() };
                        lines.push(Line::from(vec![
                            Span::styled(format!("    v{:<4}", v.number()), theme::muted()),
                            Span::styled(format!("{:<10}", v.state.to_lowercase()), st_style),
                            Span::styled(format!("{when}{destroyed}"), theme::muted()),
                        ]));
                    }
                    if vs.len() > 4 {
                        lines.push(Line::styled(format!("    … {} older", vs.len() - 4), theme::muted()));
                    }
                }
                Err(e) => lines
                    .push(Line::styled(format!("  versions unreadable: {}", e.short()), Style::new().fg(theme::WARN))),
            }
            for finding in &st.findings {
                let color = if finding.serious() { theme::ERR } else { theme::WARN };
                lines.push(Line::styled(format!("  ! {}", finding.text()), Style::new().fg(color)));
            }
            lines.push(Line::raw(""));
        }
        lines
    }

    pub(super) fn render_dialog(&self, f: &mut Frame, area: Rect, view: &View) {
        let Some(d) = &self.dialog else { return };
        let color = if self.production { theme::ERR } else { theme::ACCENT };
        d.form.render(f, area, color);
        if d.busy {
            let text = match d.action {
                super::Action::Restart => format!("Restarting {}: waiting for the new revision…", d.app),
                super::Action::Min(n) => format!("Setting the minimum of {} to {n}…", d.app),
            };
            let body = Text::from(vec![Line::raw(""), spinner_line(view.tick, &text)]);
            let width = (text.chars().count() as u16 + 8).min(area.width);
            widgets::popup(f, widgets::popup_area(area, width, 5), "Cloud Run", theme::ACCENT, body);
        }
    }
}

fn tabs_line(open: &Open, secret_alerts: usize) -> Line<'static> {
    let mut spans = Vec::new();
    for tab in TABS {
        let mut label = format!(" {} ", tab.label());
        if tab == Tab::Secrets && secret_alerts > 0 {
            label = format!(" {} ! {secret_alerts} ", tab.label());
        }
        let st = if tab == open.tab {
            Style::new().fg(Color::Black).bg(theme::ACCENT).bold()
        } else if tab == Tab::Secrets && secret_alerts > 0 {
            Style::new().fg(theme::ERR).bold()
        } else {
            theme::muted()
        };
        spans.push(Span::styled(label, st));
        spans.push(Span::raw(" "));
    }
    Line::from(spans)
}

/// What the service is configured to do, for the preview and the Health tab.
fn config_lines(s: &Service, kw: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let ready = s.revision_ready_at().map(|t| format!(" · ready {}", fmt_ago(t))).unwrap_or_default();
    let traffic = s.traffic_split().unwrap_or_else(|| "100% latest".into());
    let serving = if s.revision().is_empty() { "no revision ready".to_string() } else { s.revision().to_string() };
    lines.push(widgets::kv("Serving", format!("{serving}{ready} · traffic {traffic}"), kw));
    let manual = s.manual_instances().map(|n| format!("manual {n} · ")).unwrap_or_default();
    let max = s.max_instances().map_or("default".into(), |m| m.to_string());
    let cpu = if s.cpu_always_on() {
        Span::styled("CPU always on", Style::new().fg(theme::WARN))
    } else {
        Span::raw("CPU during requests")
    };
    let min_style = if s.min_instances() > 0 { Style::new().fg(theme::WARN) } else { Style::new() };
    lines.push(Line::from(vec![
        Span::styled(format!("{:<kw$} ", "Scaling"), theme::muted()),
        Span::styled(format!("{manual}min {}", s.min_instances()), min_style),
        Span::raw(format!(" (service {} · revision {}) · max {max} · ", s.service_min(), s.revision_min())),
        cpu,
    ]));
    let concurrency = s.template.max_instance_request_concurrency.map_or("default".into(), |c| c.to_string());
    lines.push(widgets::kv(
        "Requests",
        format!(
            "timeout {} · concurrency {concurrency} · ingress {}",
            fmt_ms(s.timeout_secs() * 1000.0),
            s.ingress_label()
        ),
        kw,
    ));
    if s.latest_not_ready() {
        let latest = s.short(run::last_segment(&s.latest_created_revision)).to_string();
        let why = s
            .conditions
            .iter()
            .chain([&s.terminal_condition])
            .find(|c| !c.kind.is_empty() && !c.succeeded() && !c.message.is_empty())
            .map(|c| format!(": {}", c.message))
            .unwrap_or_default();
        lines.push(widgets::kv(
            "Newest",
            Span::styled(format!("{latest} is not ready{why}"), Style::new().fg(theme::ERR)),
            kw,
        ));
    }
    if !s.last_modifier.is_empty() {
        let when = s.updated().map(|t| format!(" · {}", fmt_ago(t))).unwrap_or_default();
        lines.push(widgets::kv("Changed by", Span::styled(format!("{}{when}", s.last_modifier), theme::muted()), kw));
    }
    lines
}

/// The health charts of `c` for `s`, `width` columns wide.
fn chart_lines(c: &Charts, s: &Service, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let h = c.summary();
    let timeout = s.timeout_secs();
    let err = Style::new().fg(theme::ERR);
    if h.held(timeout) {
        lines.push(Line::styled(
            format!(
                "Held requests: about {} ran at the timeout ({}). {HELD_EXPLAINED}",
                fmt_count(h.near_timeout(timeout).round()),
                fmt_ms(timeout * 1000.0)
            ),
            err.bold(),
        ));
    } else if h.near_timeout(timeout) >= 1.0 {
        lines.push(Line::styled(
            format!(
                "About {} requests ran at least {:.0}% of the {} timeout (p99 is below it).",
                fmt_count(h.near_timeout(timeout).round()),
                HELD_SHARE * 100.0,
                fmt_ms(timeout * 1000.0)
            ),
            Style::new().fg(theme::WARN),
        ));
    }
    if h.requests == 0.0 && c.billed.iter().all(|p| p.1 == 0.0) {
        lines.push(Line::styled(format!("No requests in the last {}.", c.range.label()), theme::muted()));
        return lines;
    }

    let requests = c.requests();
    let mut chart = Chart::new("Requests", &requests, c.window);
    chart.stacked = Some(c.class("5xx"));
    chart.axis = false;
    let mut summary = vec![Span::styled(format!("{} total", fmt_count(h.requests)), theme::muted())];
    for (class, warn) in [("4xx", 10.0), ("5xx", 0.05)] {
        let p = h.percent(class).unwrap_or(0.0);
        let st = if class == "5xx" && p >= 1.0 {
            err
        } else if p >= warn {
            Style::new().fg(theme::WARN)
        } else {
            theme::muted()
        };
        summary.push(Span::styled(format!(" · {class} {} ({p:.1}%)", fmt_count(h.count(class))), st));
    }
    chart.summary = summary;
    lines.extend(chart.lines(width));

    let (p50, p99) = (c.percentile(0.5), c.percentile(0.99));
    let mut latency = Chart::new("Latency p50/p99", &p99, c.window);
    latency.agg = Agg::Max;
    latency.color = theme::WARN;
    // The median at the bottom of each p99 bar, on the same scale.
    latency.stacked = Some(&p50);
    latency.stacked_color = theme::ACCENT;
    latency.fmt = fmt_ms;
    latency.threshold = Some(timeout * 1000.0);
    latency.axis = false;
    let ms = |q: f64| h.percentile(q).map_or("—".into(), fmt_ms);
    let held = h.held(timeout);
    latency.summary = vec![Span::styled(
        format!("p50 {} · p95 {} · p99 {} · timeout {}", ms(0.5), ms(0.95), ms(0.99), fmt_ms(timeout * 1000.0)),
        if held { err } else { theme::muted() },
    )];
    lines.extend(latency.lines(width));

    let instances = c.instances();
    let mut chart = Chart::new("Instances", &instances, c.window);
    chart.agg = Agg::Max;
    chart.color = theme::OK;
    chart.rows = 2;
    chart.axis = false;
    let max = instances.iter().map(|p| p.1).fold(0.0, f64::max);
    chart.summary = vec![Span::styled(format!("up to {}", fmt_count(max)), theme::muted())];
    lines.extend(chart.lines(width));

    let billed = c.billed_instances();
    let mut chart = Chart::new("Billed", &billed, c.window);
    chart.agg = Agg::Mean;
    chart.color = theme::ACCENT;
    chart.rows = 2;
    chart.summary = vec![Span::styled(
        format!("{} of instance time · instances billed on average", fmt_duration(h.billed_secs)),
        theme::muted(),
    )];
    lines.extend(chart.lines(width));
    lines
}

fn revision_lines(open: &Open, s: &Service, view: &View) -> Vec<Line<'static>> {
    let revs = match &open.revisions {
        None => return vec![spinner_line(view.tick, "reading the revisions…")],
        Some(Err(e)) => return vec![Line::styled(e.short(), Style::new().fg(theme::ERR))],
        Some(Ok(r)) => r,
    };
    let mut lines =
        vec![Line::styled(format!("The {} newest revisions · values are never shown", revs.len()), theme::muted())];
    lines.push(Line::raw(""));
    for r in revs {
        let when = r.created().map(|t| format!("{} ({})", fmt_ago(t), fmt_utc(t))).unwrap_or_default();
        let mut head = vec![
            Span::styled(r.id().to_string(), theme::bold()),
            Span::styled(format!("  {when}  by {}", r.creator), theme::muted()),
        ];
        if r.id() == s.revision() {
            head.push(Span::styled("  ● serving", Style::new().fg(theme::OK)));
        }
        if r.labels.contains_key(run::RESTART_LABEL) {
            head.push(Span::styled("  ↻ gcli restart", theme::accent()));
        }
        lines.push(Line::from(head));
        lines.push(Line::from(vec![Span::styled("  image    ", theme::muted()), Span::raw(r.image_short())]));
        let names = r.plain_env_names();
        if !names.is_empty() {
            lines.push(Line::from(vec![Span::styled("  vars     ", theme::muted()), Span::raw(names.join(" "))]));
        }
        for b in r.secret_refs() {
            let how = if b.mounted { "file " } else { "" };
            lines.push(Line::from(vec![
                Span::styled("  secret   ", theme::muted()),
                Span::styled(format!("{how}{}", b.target), theme::accent()),
                Span::styled(" ← ", theme::muted()),
                Span::raw(format!("{}:{}", b.secret_id(), b.version)),
            ]));
        }
        lines.push(Line::raw(""));
    }
    lines
}
