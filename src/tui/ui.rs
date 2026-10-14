//! Rendering. Pure function of `&App`.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Bar, BarChart, BarGroup, Block, Clear, Gauge, Paragraph, Row, Sparkline, Table, TableState,
    Tabs, Wrap,
};

use super::app::*;
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
    let titles: Vec<String> = Tab::ALL
        .iter()
        .enumerate()
        .map(|(i, t)| format!("{} {}", i + 1, t.title()))
        .collect();
    f.render_widget(
        Tabs::new(titles)
            .select(app.tab.index())
            .block(
                Block::bordered()
                    .title(" 🦀 omnicrab ".fg(ACCENT).bold())
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

fn draw_chat(f: &mut Frame, app: &App, area: Rect) {
    let in_lines = app.input.split('\n').count().clamp(1, 6) as u16;
    let [msgs, input] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(in_lines + 2)]).areas(area);

    let mut lines: Vec<Line> = vec![];
    if app.entries.is_empty() {
        for l in [
            "Welcome to omnicrab.",
            "",
            "  - Tab -> Accounts, then 'a' to add one (or 'D' to auto-detect Ollama / installed CLIs)",
            "  - type a message, or @name to route to an agent/account",
            "  - /fanout a,b <prompt>   /orchestrate <task>   /help",
        ] {
            lines.push(Line::from(l.fg(Color::DarkGray)));
        }
    }
    for e in &app.entries {
        let (name_style, label) = match e.role {
            Role::User => (Style::new().fg(Color::Cyan).bold(), e.who.clone()),
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
    let shown: Vec<Line> = {
        let mut v: Vec<Line> = app
            .input
            .split('\n')
            .map(|l| Line::raw(l.to_string()))
            .collect();
        if let Some(last) = v.last_mut() {
            last.spans.push(Span::raw(cursor));
        }
        let skip = v.len().saturating_sub(6);
        v.into_iter().skip(skip).collect()
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
            "a add · e edit/rename · t test · space on/off · Enter use · d delete · D detect local",
        ),
    );
    if app.accounts.is_empty() {
        let a = centered(area, 60, 3);
        f.render_widget(Paragraph::new("No accounts yet. Press 'a' to add one, or 'D' to detect Ollama and installed CLIs.").alignment(Alignment::Center).wrap(Wrap { trim: true }), a);
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
    let [top, bottom] =
        Layout::vertical([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(area);
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
                "t window ({}) · r refresh · ↑↓ select",
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

// ---------------------------------------------------------------- modals

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
        "Enter save · Ctrl+T test · Tab next · Esc cancel"
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
    let a = centered(f.area(), 84, 30);
    f.render_widget(Clear, a);
    let mut t: Vec<Line> = vec![
        Line::from("Keys".fg(ACCENT).bold()),
        Line::raw("  F1-F5 / Tab     switch tabs         Ctrl+N new session     Ctrl+L lock vault"),
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
