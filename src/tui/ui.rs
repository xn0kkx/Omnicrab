//! Rendering. Pure function of `&App`.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Bar, BarChart, BarGroup, Block, Clear, Gauge, LineGauge, Paragraph, Row, Sparkline, Table,
    TableState, Tabs, Wrap,
};

use super::app::*;
use super::crab;
use super::form::{Form, FormKind};
use crate::model::*;
use crate::usage::Window;

const ACCENT: Color = Color::Rgb(222, 115, 61); // rust orange
const DIM: Style = Style::new().fg(Color::DarkGray);

pub fn draw(f: &mut Frame, app: &App) {
    match &app.screen {
        Screen::Unlock(u) => draw_unlock(f, u, app.tick),
        Screen::Main => draw_main(f, app),
    }
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

fn mask(n: usize) -> String {
    "•".repeat(n)
}

fn draw_unlock(f: &mut Frame, u: &UnlockState, tick: u64) {
    let area = centered(f.area(), 56, if u.first_run { 14 } else { 11 });
    let title = if u.first_run {
        " omnicrab - create vault "
    } else {
        " omnicrab - unlock "
    };
    let block = Block::bordered()
        .title(title)
        .border_style(Style::new().fg(ACCENT));
    let mut lines = vec![
        Line::from("🦀 omnicrab".fg(ACCENT).bold()).centered(),
        Line::from("all data is encrypted before it reaches PostgreSQL".fg(Color::DarkGray))
            .centered(),
        Line::raw(""),
    ];
    if u.first_run {
        lines.push(Line::from(
            "Choose a master password (min 8 chars). It cannot be recovered.".yellow(),
        ));
        lines.push(Line::raw(""));
    }
    let cur = |active: bool| if active && tick % 6 < 3 { "▏" } else { " " };
    lines.push(Line::from(vec![
        Span::styled(
            if !u.on_confirm { "> " } else { "  " },
            Style::new().fg(ACCENT),
        ),
        Span::raw("Password: "),
        Span::raw(mask(u.pw.chars().count())),
        Span::raw(cur(!u.on_confirm)),
    ]));
    if u.first_run {
        lines.push(Line::from(vec![
            Span::styled(
                if u.on_confirm { "> " } else { "  " },
                Style::new().fg(ACCENT),
            ),
            Span::raw("Confirm:  "),
            Span::raw(mask(u.confirm.chars().count())),
            Span::raw(cur(u.on_confirm)),
        ]));
    }
    lines.push(Line::raw(""));
    if u.busy {
        lines.push(Line::from("deriving key (Argon2id)...".fg(Color::Cyan)));
    } else if let Some(e) = &u.error {
        lines.push(Line::from(e.clone().red()));
    }
    lines.push(Line::from("Enter confirm - Esc quit".fg(Color::DarkGray)));
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_main(f: &mut Frame, app: &App) {
    let [tabs_a, body, status] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(5),
        Constraint::Length(1),
    ])
    .areas(f.area());
    let titles: Vec<String> = Tab::ALL.iter().map(|t| t.label()).collect();
    f.render_widget(
        Tabs::new(titles)
            .select(app.tab.index())
            .block(
                Block::bordered()
                    .title(" 🦀 omnicrab ".fg(ACCENT).bold())
                    .title(
                        Line::from(
                            " Ctrl+P switch · Ctrl+←/→ prev/next · click ".fg(Color::DarkGray),
                        )
                        .right_aligned(),
                    )
                    .border_style(DIM),
            )
            .highlight_style(Style::new().fg(ACCENT).bold().underlined()),
        tabs_a,
    );
    match app.tab {
        Tab::Chat => draw_chat(f, app, body),
        Tab::Accounts => draw_accounts(f, app, body),
        Tab::Agents => draw_agents(f, app, body),
        Tab::Usage => draw_usage(f, app, body),
        Tab::Sessions => draw_sessions(f, app, body),
        Tab::Routing => draw_routing(f, app, body),
    }
    draw_status(f, app, status);
    match &app.modal {
        Some(Modal::Form(form)) => draw_form(f, form),
        Some(Modal::Confirm { text, .. }) => {
            let a = centered(f.area(), 60, 5);
            f.render_widget(Clear, a);
            f.render_widget(
                Paragraph::new(vec![
                    Line::raw(text.clone()),
                    Line::raw(""),
                    Line::from("y / Enter = yes    n / Esc = no".fg(Color::DarkGray)),
                ])
                .block(
                    Block::bordered()
                        .title(" Confirm ")
                        .border_style(Style::new().fg(Color::Red)),
                )
                .wrap(Wrap { trim: true }),
                a,
            );
        }
        Some(Modal::Help) => draw_help(f),
        Some(Modal::Switcher(sel)) => draw_switcher(f, *sel, app.tab),
        Some(Modal::Picker(p)) => draw_picker(f, p),
        Some(Modal::Plan) => {
            if let Some(pr) = &app.pending_plan {
                draw_plan(f, pr)
            }
        }
        None => {}
    }
}

fn target_label(app: &App) -> String {
    match app.target {
        Some(Target::Account(id)) => app
            .accounts
            .iter()
            .find(|a| a.id == id)
            .map(|a| format!("account {}", a.name)),
        Some(Target::Agent(id)) => app
            .agents
            .iter()
            .find(|a| a.id == id)
            .map(|a| format!("agent @{}", a.name)),
        None => None,
    }
    .unwrap_or_else(|| "no target".into())
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let spinner = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"][(app.tick % 10) as usize];
    let mut spans = vec![Span::styled(
        format!(" {} ", target_label(app)),
        Style::new().bg(ACCENT).fg(Color::Black).bold(),
    )];
    let (label, color) = match app.mode {
        Mode::Normal => ("", Color::Reset),
        Mode::Plan => (" PLAN ", Color::Cyan),
        Mode::Auto => (" AUTO ", Color::Magenta),
    };
    if !label.is_empty() {
        spans.push(Span::styled(
            label,
            Style::new().bg(color).fg(Color::Black).bold(),
        ));
    }
    if let Some(m) = &app.model_override {
        spans.push(Span::raw(format!(" model={m}")));
    }
    spans.push(Span::styled(
        format!(
            "  session: {} tok  ${:.4}",
            app.session_tokens, app.session_cost
        ),
        DIM,
    ));
    if app.active_runs > 0 {
        spans.push(Span::styled(
            format!("  {spinner} working (Esc to cancel)"),
            Style::new().fg(Color::Cyan),
        ));
    }
    if let Some(n) = app.notice.as_ref().filter(|n| !n.is_empty()) {
        spans.push(Span::styled(
            format!("  - {n}"),
            Style::new().fg(Color::Yellow),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

// ---------------------------------------------------------------- chat

fn wrapped_height(text: &str, width: usize) -> usize {
    let w = width.max(1);
    text.split('\n')
        .map(|l| l.chars().count().max(1).div_ceil(w))
        .sum()
}

fn draw_crab(f: &mut Frame, app: &App, area: Rect) {
    let running = app.active_runs > 0;
    let (pose, offset) = crab::frame(app.tick, running, area.width as usize);
    let pad = " ".repeat(offset);
    let style = Style::new().fg(if running {
        ACCENT
    } else {
        Color::Rgb(150, 80, 45)
    });
    let lines: Vec<Line> = pose
        .iter()
        .map(|l| Line::from(Span::styled(format!("{pad}{l}"), style)))
        .collect();
    f.render_widget(Paragraph::new(lines), area);
}

fn draw_chat(f: &mut Frame, app: &App, area: Rect) {
    let in_lines = app.input.as_str().split('\n').count().clamp(1, 6) as u16;
    // The crab needs its own strip; skip it on short terminals.
    let crab_h = if area.height >= 18 {
        crab::HEIGHT as u16
    } else {
        0
    };
    let [msgs, crab_a, input] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(crab_h),
        Constraint::Length(in_lines + 2),
    ])
    .areas(area);
    if crab_h > 0 {
        draw_crab(f, app, crab_a);
    }

    let mut lines: Vec<Line> = vec![];
    if app.entries.is_empty() {
        for l in [
            "Welcome to omnicrab.",
            "",
            "  - Tab -> Accounts, then 'a' to add one (or 'D' to auto-detect installed CLIs)",
            "  - type a message, or @name to route to an agent/account",
            "  - /fanout a,b <prompt>   /orchestrate <task>   /help",
        ] {
            lines.push(Line::from(l.fg(Color::DarkGray)));
        }
    }
    for e in &app.entries {
        let (name_style, label) = match e.role {
            Role::User => (Style::new().fg(Color::Cyan).bold(), e.who.clone()),
            Role::System => (Style::new().fg(Color::Yellow).bold(), e.who.clone()),
            _ => (
                Style::new()
                    .fg(if e.failed { Color::Red } else { Color::Green })
                    .bold(),
                e.who.clone(),
            ),
        };
        let mut head = vec![Span::styled(label, name_style)];
        if e.streaming {
            head.push(Span::styled(" ▍", Style::new().fg(Color::Cyan)));
        }
        for n in &e.notes {
            head.push(Span::styled(
                format!("  {n}"),
                Style::new().fg(Color::Yellow).add_modifier(Modifier::DIM),
            ));
        }
        lines.push(Line::from(head));
        for l in e.text.split('\n') {
            lines.push(Line::raw(l.to_string()));
        }
        if !e.meta.is_empty() {
            lines.push(Line::from(Span::styled(e.meta.clone(), DIM)));
        }
        lines.push(Line::raw(""));
    }
    let inner_w = msgs.width.saturating_sub(2) as usize;
    let total: usize = lines
        .iter()
        .map(|l| {
            wrapped_height(
                &l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>(),
                inner_w,
            )
        })
        .sum();
    let h = msgs.height.saturating_sub(2) as usize;
    let max_up = total.saturating_sub(h);
    let up = (app.scroll_up as usize).min(max_up);
    let y = (max_up - up) as u16;
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((y, 0))
            .block(Block::bordered().border_style(DIM)),
        msgs,
    );

    let cursor = if app.tick % 6 < 3 { "▏" } else { " " };
    let (cur_line, cur_col) = app.input.line_col();
    let shown: Vec<Line> = {
        let v: Vec<Line> = app
            .input
            .as_str()
            .split('\n')
            .enumerate()
            .map(|(i, l)| {
                if i == cur_line {
                    let before: String = l.chars().take(cur_col).collect();
                    let after: String = l.chars().skip(cur_col).collect();
                    Line::from(vec![Span::raw(before), Span::raw(cursor), Span::raw(after)])
                } else {
                    Line::raw(l.to_string())
                }
            })
            .collect();
        // Keep the line with the cursor in the 6-line window.
        let skip = (cur_line + 1).saturating_sub(6);
        v.into_iter().skip(skip).take(6).collect()
    };
    f.render_widget(
        Paragraph::new(shown).wrap(Wrap { trim: false }).block(
            Block::bordered()
                .title(" message (Enter send · Alt+Enter newline) ")
                .border_style(Style::new().fg(ACCENT)),
        ),
        input,
    );

    let comps = app.completions();
    if !comps.is_empty() {
        let n = comps.len().min(8) as u16;
        let a = Rect::new(
            input.x + 1,
            input.y.saturating_sub(n + 2),
            input.width.saturating_sub(2).min(70),
            n + 2,
        );
        f.render_widget(Clear, a);
        let items: Vec<Line> = comps
            .iter()
            .take(8)
            .map(|(c, d)| {
                Line::from(vec![
                    Span::styled(c.clone(), Style::new().fg(ACCENT).bold()),
                    Span::styled(format!("  {d}"), DIM),
                ])
            })
            .collect();
        f.render_widget(
            Paragraph::new(items).block(
                Block::bordered()
                    .title(" Tab to complete ")
                    .border_style(DIM),
            ),
            a,
        );
    }
}

// ---------------------------------------------------------------- lists

fn money(v: f64) -> String {
    if v >= 100.0 {
        format!("${v:.0}")
    } else if v >= 1.0 {
        format!("${v:.2}")
    } else {
        format!("${v:.4}")
    }
}

fn short_n(n: u64) -> String {
    match n {
        0..=9_999 => n.to_string(),
        10_000..=9_999_999 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

fn table_block(title: &str, hint: &str) -> Block<'static> {
    Block::bordered()
        .title(format!(" {title} "))
        .title_bottom(Line::from(format!(" {hint} ").fg(Color::DarkGray)))
        .border_style(DIM)
}

fn render_table(
    f: &mut Frame,
    area: Rect,
    header: Vec<&str>,
    rows: Vec<Row>,
    widths: Vec<Constraint>,
    sel: usize,
    block: Block,
) {
    let t = Table::new(rows, widths)
        .header(Row::new(header).style(Style::new().fg(ACCENT).bold()))
        .row_highlight_style(Style::new().bg(Color::Rgb(50, 50, 60)).bold())
        .highlight_symbol("▶ ")
        .block(block);
    f.render_stateful_widget(t, area, &mut TableState::default().with_selected(Some(sel)));
}

fn draw_accounts(f: &mut Frame, app: &App, area: Rect) {
    let rows: Vec<Row> = app
        .accounts
        .iter()
        .map(|a| {
            let u = app.usage.get(&a.id);
            let status = if !a.enabled {
                "off".fg(Color::DarkGray)
            } else {
                "on".fg(Color::Green)
            };
            let active = matches!(app.target, Some(Target::Account(id)) if id == a.id);
            let budget = a
                .budget_usd_month
                .map(|b| {
                    format!(
                        "{} / {}",
                        money(u.map(|u| u.month_cost).unwrap_or(0.0)),
                        money(b)
                    )
                })
                .unwrap_or_else(|| money(u.map(|u| u.month_cost).unwrap_or(0.0)));
            let key = a
                .api_key
                .as_ref()
                .map(|k| {
                    use secrecy::ExposeSecret;
                    mask_secret(k.expose_secret())
                })
                .unwrap_or_else(|| "-".into());
            Row::new(vec![
                Line::from(format!("{}{}", if active { "★ " } else { "" }, a.name)),
                Line::from(a.kind.short().to_string()),
                Line::from(if a.model.is_empty() {
                    "(default)".to_string()
                } else {
                    a.model.clone()
                }),
                Line::from(key),
                Line::from(status),
                Line::from(budget),
            ])
        })
        .collect();
    render_table(
        f,
        area,
        vec!["Name", "Provider", "Model", "Key", "State", "Spent (month)"],
        rows,
        vec![
            Constraint::Min(18),
            Constraint::Length(14),
            Constraint::Min(18),
            Constraint::Length(12),
            Constraint::Length(6),
            Constraint::Length(18),
        ],
        app.sel[1],
        table_block(
            "Accounts",
            "a add · e edit/rename · t test · space on/off · Enter use · d delete · D detect CLIs",
        ),
    );
    if app.accounts.is_empty() {
        let a = centered(area, 60, 3);
        f.render_widget(
            Paragraph::new(
                "No accounts yet. Press 'a' to add one, or 'D' to detect installed CLIs.",
            )
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
            a,
        );
    }
}

fn draw_agents(f: &mut Frame, app: &App, area: Rect) {
    let rows: Vec<Row> = app
        .agents
        .iter()
        .map(|a| {
            let acc = app
                .accounts
                .iter()
                .find(|x| x.id == a.account_id)
                .map(|x| x.name.clone())
                .unwrap_or_else(|| "?".into());
            let fb = a
                .fallback_account_ids
                .iter()
                .filter_map(|id| app.accounts.iter().find(|x| x.id == *id))
                .map(|x| x.name.clone())
                .collect::<Vec<_>>()
                .join(", ");
            Row::new(vec![
                format!("@{}", a.name),
                a.role.clone(),
                acc,
                a.model.clone().unwrap_or_default(),
                fb,
            ])
        })
        .collect();
    render_table(
        f,
        area,
        vec!["Agent", "Role", "Account", "Model", "Fallbacks"],
        rows,
        vec![
            Constraint::Length(16),
            Constraint::Min(24),
            Constraint::Length(20),
            Constraint::Length(18),
            Constraint::Min(16),
        ],
        app.sel[2],
        table_block("Agents", "a add · e edit · Enter use · d delete"),
    );
}

fn draw_sessions(f: &mut Frame, app: &App, area: Rect) {
    let rows: Vec<Row> = app
        .sessions
        .iter()
        .map(|s| {
            let cur = if app.session_id == Some(s.id) {
                "● "
            } else {
                ""
            };
            Row::new(vec![
                format!("{cur}{}", s.title),
                s.created_at
                    .with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string(),
            ])
        })
        .collect();
    render_table(
        f,
        area,
        vec!["Title", "Created"],
        rows,
        vec![Constraint::Min(30), Constraint::Length(18)],
        app.sel[4],
        table_block("Sessions", "Enter open · n new · d delete"),
    );
}

// ---------------------------------------------------------------- usage

fn draw_usage(f: &mut Frame, app: &App, area: Rect) {
    let limits_h = app.accounts.len().clamp(1, 6) as u16 + 2;
    let [top, limits, bottom] = Layout::vertical([
        Constraint::Percentage(35),
        Constraint::Length(limits_h),
        Constraint::Min(8),
    ])
    .areas(area);
    draw_limits(f, app, limits);
    let w = app.window;
    let rows: Vec<Row> = app
        .accounts
        .iter()
        .map(|a| {
            let t = app
                .usage
                .get(&a.id)
                .map(|u| u.window(w).clone())
                .unwrap_or_default();
            Row::new(vec![
                a.name.clone(),
                t.requests.to_string(),
                t.errors.to_string(),
                short_n(t.input),
                short_n(t.output),
                money(t.cost),
            ])
        })
        .collect();
    let total: f64 = app
        .accounts
        .iter()
        .filter_map(|a| app.usage.get(&a.id))
        .map(|u| u.window(w).cost)
        .sum();
    render_table(
        f,
        top,
        vec![
            "Account",
            "Requests",
            "Errors",
            "Tokens in",
            "Tokens out",
            "Cost",
        ],
        rows,
        vec![
            Constraint::Min(20),
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Length(11),
            Constraint::Length(11),
            Constraint::Length(12),
        ],
        app.sel[3],
        table_block(
            &format!("Usage - {}  (total {})", w.label(), money(total)),
            &format!(
                "t window ({}) · r refresh · p probe limits · ↑↓ select",
                Window::ALL
                    .iter()
                    .map(|x| x.label())
                    .collect::<Vec<_>>()
                    .join("/")
            ),
        ),
    );

    let [left, right] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(bottom);
    // Left: tokens/day sparkline + budget gauge for the selected account.
    let sel = app.accounts.get(app.sel[3]);
    let lb = Block::bordered()
        .title(format!(
            " {} - tokens/day, last 30d ",
            sel.map(|a| a.name.as_str()).unwrap_or("-")
        ))
        .border_style(DIM);
    let inner = lb.inner(left);
    f.render_widget(lb, left);
    let [spark, gauge] = Layout::vertical([Constraint::Min(2), Constraint::Length(2)]).areas(inner);
    if let Some(a) = sel {
        let u = app.usage.get(&a.id);
        let data: Vec<u64> = u
            .map(|u| u.daily_tokens.clone())
            .unwrap_or_else(|| vec![0; 30]);
        f.render_widget(
            Sparkline::default()
                .data(data)
                .style(Style::new().fg(ACCENT)),
            spark,
        );
        let spent = u.map(|u| u.month_cost).unwrap_or(0.0);
        match a.budget_usd_month {
            Some(b) if b > 0.0 => {
                let r = (spent / b).clamp(0.0, 1.0);
                let color = if r > 0.9 {
                    Color::Red
                } else if r > 0.7 {
                    Color::Yellow
                } else {
                    Color::Green
                };
                f.render_widget(
                    Gauge::default()
                        .ratio(r)
                        .label(format!("budget {} / {}", money(spent), money(b)))
                        .gauge_style(Style::new().fg(color)),
                    gauge,
                );
            }
            _ => f.render_widget(
                Paragraph::new(Line::from(
                    format!("month spend {} (no budget set)", money(spent)).fg(Color::DarkGray),
                )),
                gauge,
            ),
        }
    }
    // Right: cost per account, bars.
    let names: Vec<String> = app
        .accounts
        .iter()
        .map(|a| a.name.chars().take(10).collect())
        .collect();
    let bars: Vec<Bar> = app
        .accounts
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let c = app
                .usage
                .get(&a.id)
                .map(|u| u.window(w).cost)
                .unwrap_or(0.0);
            Bar::default()
                .value((c * 10_000.0) as u64)
                .label(Line::from(names[i].clone()))
                .text_value(money(c))
                .style(Style::new().fg(ACCENT))
        })
        .collect();
    f.render_widget(
        BarChart::default()
            .data(BarGroup::default().bars(&bars))
            .bar_width(10)
            .bar_gap(1)
            .block(
                Block::bordered()
                    .title(format!(" cost by account ({}) ", w.label()))
                    .border_style(DIM),
            ),
        right,
    );
}

fn pct_color(r: f64) -> Color {
    if r > 0.9 {
        Color::Red
    } else if r > 0.7 {
        Color::Yellow
    } else {
        Color::Green
    }
}

fn until(now: chrono::DateTime<chrono::Utc>, t: chrono::DateTime<chrono::Utc>) -> String {
    let m = (t - now).num_minutes().max(0);
    match m {
        0..=59 => format!("{m}m"),
        60..=1439 => format!("{}h{:02}m", m / 60, m % 60),
        _ => format!("{}d {}h", m / 1440, m % 1440 / 60),
    }
}

fn limit_gauge(
    label: &str,
    w: Option<&LimitWindow>,
    now: chrono::DateTime<chrono::Utc>,
) -> LineGauge<'static> {
    use crate::usage::LimitState;
    let (ratio, text, color) = match w.map(|w| LimitState::of(w, now)) {
        Some(LimitState::Active { used, resets_at }) => (
            used.clamp(0.0, 1.0),
            format!("{label} {:>3.0}% ↻{}", used * 100.0, until(now, resets_at)),
            pct_color(used),
        ),
        Some(LimitState::Reset) => (0.0, format!("{label}   0% (reset)"), Color::Green),
        None => (0.0, format!("{label}  --"), Color::DarkGray),
    };
    LineGauge::default()
        .ratio(ratio)
        .label(text)
        .filled_style(Style::new().fg(color))
        .unfilled_style(DIM)
}

/// Plan-limit usage (5-hour and weekly windows) per account.
fn draw_limits(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::bordered()
        .title(" Plan limits - 5h / weekly ")
        .title(Line::from(" Claude CLI accounts · p refresh ".fg(Color::DarkGray)).right_aligned())
        .border_style(DIM);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if app.accounts.is_empty() {
        f.render_widget(Paragraph::new("no accounts yet".fg(Color::DarkGray)), inner);
        return;
    }
    for (i, a) in app.accounts.iter().take(inner.height as usize).enumerate() {
        let row = Rect::new(inner.x, inner.y + i as u16, inner.width, 1);
        let [name, g5, g7, age] = Layout::horizontal([
            Constraint::Length(20),
            Constraint::Fill(1),
            Constraint::Fill(1),
            Constraint::Length(12),
        ])
        .areas(row);
        let style = if i == app.sel[3] {
            Style::new().fg(ACCENT).bold()
        } else {
            Style::new()
        };
        let n: String = a.name.chars().take(19).collect();
        f.render_widget(Paragraph::new(Span::styled(n, style)), name);
        if a.kind != ProviderKind::ClaudeCli {
            f.render_widget(
                Paragraph::new("n/a (no plan limits reported)".fg(Color::DarkGray)),
                g5,
            );
            continue;
        }
        match app.usage.get(&a.id).and_then(|u| u.limits.as_ref()) {
            Some((l, at)) => {
                f.render_widget(limit_gauge("5h", l.five_hour.as_ref(), app.now), g5);
                f.render_widget(limit_gauge("7d", l.seven_day.as_ref(), app.now), g7);
                f.render_widget(
                    Paragraph::new(format!(" {} ago", until(*at, app.now)).fg(Color::DarkGray)),
                    age,
                );
            }
            None => f.render_widget(
                Paragraph::new("no data yet - send a message or press p".fg(Color::DarkGray)),
                g5,
            ),
        }
    }
}

// ---------------------------------------------------------------- modals

fn draw_switcher(f: &mut Frame, sel: usize, current: Tab) {
    let a = centered(f.area(), 34, Tab::ALL.len() as u16 + 4);
    f.render_widget(Clear, a);
    let mut lines: Vec<Line> = Tab::ALL
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let mark = if *t == current { "•" } else { " " };
            let style = if i == sel {
                Style::new().bg(ACCENT).fg(Color::Black).bold()
            } else {
                Style::new()
            };
            Line::from(Span::styled(
                format!(" {mark} [{}] {:<10} {}", t.mnemonic(), t.title(), i + 1),
                style,
            ))
        })
        .collect();
    lines.push(Line::raw(""));
    lines.push(Line::from(
        " letter/number · ↑↓ Enter · Esc".fg(Color::DarkGray),
    ));
    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(" Go to ")
                .border_style(Style::new().fg(ACCENT)),
        ),
        a,
    );
}

fn draw_form(f: &mut Frame, form: &Form) {
    let vis = form.visible_indices();
    let h = (vis.len() as u16) * 2 + 5;
    let a = centered(f.area(), 78, h);
    f.render_widget(Clear, a);
    let mut lines: Vec<Line> = vec![];
    for &i in &vis {
        let fld = &form.fields[i];
        let focused = i == form.focus;
        let value = if fld.is_choice() {
            format!("◀ {} ▶", fld.choices[fld.choice])
        } else if fld.secret {
            mask(fld.value.chars().count())
        } else {
            fld.value.clone()
        };
        let cursor = if focused && !fld.is_choice() {
            "▏"
        } else {
            ""
        };
        lines.push(Line::from(vec![
            Span::styled(if focused { "> " } else { "  " }, Style::new().fg(ACCENT)),
            Span::styled(
                format!("{:<15}", fld.label),
                if focused {
                    Style::new().bold()
                } else {
                    Style::new()
                },
            ),
            Span::raw(format!("{value}{cursor}")),
        ]));
        lines.push(Line::from(Span::styled(
            format!("    {:<15}{}", "", fld.hint),
            DIM,
        )));
    }
    lines.push(Line::raw(""));
    let keys = if matches!(form.kind, FormKind::Account { .. }) {
        "Enter save · Ctrl+T test · Ctrl+O pick model · Tab next · Esc cancel"
    } else if matches!(form.kind, FormKind::Agent { .. }) {
        "Enter save · Ctrl+O pick model · Tab next · Esc cancel"
    } else {
        "Enter save · Tab next · Esc cancel"
    };
    lines.push(Line::from(match &form.status {
        Some(s) => Span::styled(
            s.clone(),
            Style::new().fg(if s.starts_with("OK") {
                Color::Green
            } else if s.starts_with("FAILED") {
                Color::Red
            } else {
                Color::Yellow
            }),
        ),
        None => Span::styled(keys, DIM),
    }));
    f.render_widget(
        Paragraph::new(lines)
            .block(
                Block::bordered()
                    .title(format!(" {} ", form.title))
                    .border_style(Style::new().fg(ACCENT)),
            )
            .wrap(Wrap { trim: false }),
        a,
    );
}

fn draw_help(f: &mut Frame) {
    let a = centered(f.area(), 96, 37);
    f.render_widget(Clear, a);
    let mut t: Vec<Line> = vec![
        Line::from("Keys".fg(ACCENT).bold()),
        Line::raw(
            "  Ctrl+P          quick tab switcher (c/a/g/u/s or 1-5)   Ctrl+←/→ prev/next tab",
        ),
        Line::raw("  F1-F6 / Tab     switch tabs · click a tab with the mouse"),
        Line::raw(
            "  Shift+Tab       cycle chat / plan / auto mode · Ctrl+O or /plan toggles plan · /model picker",
        ),
        Line::raw(
            "  Routing tab:    a add · m model · f effort · Space on/off · J/K move · d remove · +/- policy",
        ),
        Line::raw("  Ctrl+N          new session         Ctrl+L lock vault"),
        Line::raw("  Usage tab:      t window · r reload · p refresh plan limits of the account"),
        Line::raw("  Esc             cancel running requests · Ctrl+C cancel, or quit when idle"),
        Line::raw("  PgUp/PgDn       scroll chat         Alt+Enter newline in message"),
        Line::raw(
            "  Lists:          ↑↓/jk select · a add · e edit · d delete · Enter use · t test (accounts)",
        ),
        Line::raw(""),
        Line::from("Routing".fg(ACCENT).bold()),
        Line::raw("  @name <msg>     send one message to an agent or account"),
        Line::raw(""),
        Line::from("Commands".fg(ACCENT).bold()),
    ];
    t.extend(COMMANDS.iter().map(|(c, d)| {
        Line::from(vec![
            Span::styled(format!("  {c:<14}"), Style::new().fg(Color::Cyan)),
            Span::raw(d.to_string()),
        ])
    }));
    t.push(Line::raw(""));
    t.push(Line::from("press any key".fg(Color::DarkGray)));
    f.render_widget(
        Paragraph::new(Text::from(t)).block(
            Block::bordered()
                .title(" Help ")
                .border_style(Style::new().fg(ACCENT)),
        ),
        a.inner(Margin::new(0, 0)),
    );
}

// ---------------------------------------------------------------- routing

fn room_bar(room: Option<f64>) -> Span<'static> {
    match room {
        Some(r) => {
            let n = (r.clamp(0.0, 1.0) * 10.0).round() as usize;
            Span::styled(
                format!(
                    "{}{} {:>3.0}% free",
                    "█".repeat(n),
                    "░".repeat(10 - n),
                    r * 100.0
                ),
                Style::new().fg(pct_color(1.0 - r)),
            )
        }
        None => Span::styled("░░░░░░░░░░ n/a", DIM),
    }
}

fn draw_routing(f: &mut Frame, app: &App, area: Rect) {
    let rows = app.routing_rows();
    let sel = app.sel[Tab::Routing.index()];
    let hints = [
        "plans the work; keep this strong",
        "pool for steps without a named agent; spread by free capacity",
        "merges results (empty = planner); skipped when there is one step",
        "summaries of old messages; a local model is ideal",
    ];
    let p = &app.routing.policy;
    let mut lines: Vec<Line> = vec![];
    for (n, row) in rows.iter().enumerate() {
        let hi = if n == sel {
            Style::new().bg(Color::Rgb(50, 50, 60)).bold()
        } else {
            Style::new()
        };
        let mark = if n == sel { "▶ " } else { "  " };
        let line = match *row {
            RoutingRow::Header(s) => {
                let count = app.routing.section(s).len();
                Line::from(vec![
                    Span::styled(mark, hi),
                    Span::styled(
                        format!("{} ({count})", ROUTING_SECTIONS[s]),
                        Style::new().fg(ACCENT).bold(),
                    ),
                    Span::styled(format!("  {}", hints[s]), DIM),
                ])
            }
            RoutingRow::Slot(s, i) => {
                let slot = &app.routing.section(s)[i];
                let acct = app.accounts.iter().find(|a| a.id == slot.account_id);
                let name = acct.map(|a| a.name.as_str()).unwrap_or("?");
                let room = acct.and_then(|a| {
                    crate::orchestrator::balance::headroom(a, app.usage.get(&a.id), app.now)
                });
                let model = slot
                    .model
                    .clone()
                    .or_else(|| acct.map(|a| a.model.clone()).filter(|m| !m.is_empty()))
                    .unwrap_or_else(|| "(cli default)".into());
                let effort = slot
                    .effort
                    .as_deref()
                    .map(|e| format!(" effort={e}"))
                    .unwrap_or_default();
                let base = if slot.enabled { Style::new() } else { DIM };
                Line::from(vec![
                    Span::styled(mark, hi),
                    Span::styled(
                        format!(
                            "  {} {:<22} {:<28}{effort:<14} ",
                            if slot.enabled { "●" } else { "○" },
                            name,
                            model
                        ),
                        base,
                    ),
                    room_bar(room),
                ])
            }
            RoutingRow::Policy(n) => {
                let (label, value) = match n {
                    0 => (
                        "skip accounts above",
                        format!("{:.0}% used", p.skip_above * 100.0),
                    ),
                    1 => (
                        "prefer local models",
                        (if p.prefer_local { "yes" } else { "no" }).into(),
                    ),
                    2 => (
                        "summarize history above",
                        format!("~{} tokens", p.history_budget_tokens),
                    ),
                    _ => ("keep last turns verbatim", p.keep_last_turns.to_string()),
                };
                Line::from(vec![
                    Span::styled(mark, hi),
                    Span::styled(
                        if n == 0 { "Policy  " } else { "        " },
                        Style::new().fg(ACCENT).bold(),
                    ),
                    Span::raw(format!("{label:<26}{value}")),
                ])
            }
        };
        lines.push(line);
    }
    let visible = area.height.saturating_sub(2) as usize;
    let skip = (sel + 1).saturating_sub(visible);
    f.render_widget(
        Paragraph::new(lines.into_iter().skip(skip).collect::<Vec<_>>()).block(table_block(
            "Routing",
            "a add · m model · f effort · Space on/off · J/K move · d remove · +/- policy · ?",
        )),
        area,
    );
}

fn draw_picker(f: &mut Frame, p: &Picker) {
    let items = p.visible();
    let h = (items.len().min(14) as u16 + 5).max(7);
    let a = centered(f.area(), 70, h);
    f.render_widget(Clear, a);
    let mut lines = vec![Line::from(vec![
        Span::styled("filter: ", DIM),
        Span::raw(format!("{}▏", p.filter)),
    ])];
    let skip = (p.sel + 1).saturating_sub(14);
    for (i, (label, _)) in items.iter().enumerate().skip(skip).take(14) {
        let style = if i == p.sel {
            Style::new().bg(ACCENT).fg(Color::Black).bold()
        } else {
            Style::new()
        };
        lines.push(Line::from(Span::styled(format!(" {label}"), style)));
    }
    if items.is_empty() {
        lines.push(Line::from(Span::styled(
            if p.filter.is_empty() {
                " (nothing to pick)".to_string()
            } else {
                format!(" no match - Enter uses \"{}\" as typed", p.filter)
            },
            DIM,
        )));
    }
    lines.push(Line::from(
        " type to filter · ↑↓ · Enter · Esc".fg(Color::DarkGray),
    ));
    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(format!(" {} ", p.title))
                .border_style(Style::new().fg(ACCENT)),
        ),
        a,
    );
}

fn draw_plan(f: &mut Frame, pr: &PlanReview) {
    let a = centered(
        f.area(),
        90,
        (pr.steps.len() as u16 * 5 + 4).min(f.area().height),
    );
    f.render_widget(Clear, a);
    let mut lines: Vec<Line> = vec![];
    for (i, s) in pr.steps.iter().enumerate() {
        let hi = if i == pr.sel {
            Style::new().bg(Color::Rgb(50, 50, 60)).bold()
        } else {
            Style::new()
        };
        let who = if s.agent.is_empty() {
            "executor pool".to_string()
        } else {
            format!("@{}", s.agent)
        };
        lines.push(Line::from(vec![
            Span::styled(if i == pr.sel { "▶ " } else { "  " }, hi),
            Span::styled(
                format!("{}. {who}", i + 1),
                Style::new().fg(Color::Cyan).bold(),
            ),
        ]));
        lines.push(Line::from(Span::styled(format!("     {}", s.task), hi)));
        lines.push(Line::raw(""));
    }
    f.render_widget(
        Paragraph::new(lines)
            .block(
                Block::bordered()
                    .title(" Plan review ")
                    .title_bottom(Line::from(
                        " Enter approve · e edit · d remove · r revise · ↑↓ · Esc discard "
                            .fg(Color::DarkGray),
                    ))
                    .border_style(Style::new().fg(Color::Cyan)),
            )
            .wrap(Wrap { trim: false }),
        a,
    );
}
