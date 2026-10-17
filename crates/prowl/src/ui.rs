//! Rendering: a pure function of [`App`].

use prowld::AuthState;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap};

use crate::app::{App, PER_PAGE, Screen, Tab};
use crate::theme;

pub fn draw(app: &App, frame: &mut Frame) {
    let [title, body, notice, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(5),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    frame.render_widget(
        Line::from(vec![
            theme::badge(" PRowl "),
            Span::styled(
                " pull-request reviews, around the clock",
                Style::default().fg(theme::MUTED),
            ),
        ]),
        title,
    );
    match app.screen {
        Screen::Connecting => centered(frame, body, "Connecting to prowld…", vec![]),
        Screen::DaemonDown => daemon_down(app, frame, body),
        Screen::Doctor => doctor(app, frame, body),
        Screen::Login => login(app, frame, body),
        Screen::Repos => {
            let block = Block::bordered()
                .border_style(theme::border())
                .title(Span::styled(
                    " Choose repositories to watch ",
                    theme::accent(),
                ));
            let inner = block.inner(body);
            frame.render_widget(block, body);
            repo_picker(app, frame, inner, true);
        }
        Screen::PolicySetup => policy_setup(frame, body),
        Screen::Main => main_screen(app, frame, body),
    }
    if let Some(text) = &app.notice {
        frame.render_widget(Line::from(text.as_str()).yellow(), notice);
    }
    status_bar(app, frame, status);
    if app.help {
        help(frame, body);
    }
    if theme::no_color() {
        theme::strip_colors(frame.buffer_mut());
    }
}

fn centered(frame: &mut Frame, area: Rect, title: &str, mut lines: Vec<Line<'static>>) {
    lines.insert(0, Line::from(title.to_owned()).bold());
    lines.insert(1, Line::from(""));
    // Long lines (paths, errors) wrap, so size the box by the rows it really takes.
    let paragraph = Paragraph::new(lines).centered().wrap(Wrap { trim: false });
    let height = (paragraph.line_count(area.width) as u16).min(area.height);
    let [_, middle, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(height),
        Constraint::Fill(1),
    ])
    .areas(area);
    frame.render_widget(paragraph, middle);
}

fn daemon_down(app: &App, frame: &mut Frame, area: Rect) {
    let mut lines = vec![match &app.daemon_error {
        // The error already names the endpoint.
        Some(e) => Line::from(e.clone()).dim(),
        None => Line::from(format!("Can't reach prowld at {}", app.endpoint)).dim(),
    }];
    lines.push(Line::from(""));
    lines.push(Line::from("PRowl's background service isn't running."));
    lines.push(Line::from(vec![
        crate::theme::key("[i] "),
        "install and start it  ".into(),
        crate::theme::key("[r] "),
        "retry  ".into(),
        crate::theme::key("[q] "),
        "quit".into(),
    ]));
    centered(frame, area, "prowld is not running", lines);
}

fn doctor(app: &App, frame: &mut Frame, area: Rect) {
    let mut lines = Vec::new();
    if let Some(d) = &app.doctor {
        let check = |name: &str, ok: bool, detail: Option<&str>| {
            Line::from(vec![
                if ok { "  ✓ ".green() } else { "  ✗ ".red() },
                Span::raw(format!("{name}  ")),
                Span::raw(detail.unwrap_or("").to_owned()).dim(),
            ])
        };
        lines.push(check(
            "claude CLI",
            d.claude.tool.problem.is_none(),
            d.claude.tool.version.as_deref(),
        ));
        if let Some(p) = &d.claude.tool.problem {
            lines.push(Line::from(format!("      {p}")).yellow());
        }
        lines.push(check(
            "git",
            d.git.problem.is_none(),
            d.git.version.as_deref(),
        ));
        if let Some(p) = &d.git.problem {
            lines.push(Line::from(format!("      {p}")).yellow());
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        crate::theme::key("[r] "),
        "check again  ".into(),
        crate::theme::key("[enter] "),
        "continue anyway  ".into(),
        crate::theme::key("[q] "),
        "quit".into(),
    ]));
    centered(frame, area, "PRowl needs a couple of tools", lines);
}

fn login(app: &App, frame: &mut Frame, area: Rect) {
    if let Some(token) = &app.token_input {
        let mut lines = vec![
            Line::from("Paste a personal access token with the repo and read:org scopes."),
            Line::from("PRowl checks it with GitHub, then keeps it in your OS keychain."),
            Line::from(""),
            Line::from(vec![
                "token: ".bold(),
                Span::raw(format!("{}▏", "•".repeat(token.chars().count()))),
            ]),
            Line::from(""),
        ];
        if let Some(error) = &app.login_error {
            lines.push(Line::from(error.clone()).red());
            lines.push(Line::from(""));
        }
        lines.push(Line::from(vec![
            crate::theme::key("[enter] "),
            "sign in  ".into(),
            crate::theme::key("[esc] "),
            "back".into(),
        ]));
        centered(frame, area, "Sign in with a token", lines);
        return;
    }
    match &app.auth {
        Some(AuthState::Pending {
            user_code,
            verification_uri,
            ..
        }) => {
            let mut lines = vec![
                Line::from(vec![
                    "Open ".into(),
                    verification_uri.clone().bold().underlined(),
                ]),
                Line::from(vec![
                    "and enter the code ".into(),
                    user_code.clone().bold().cyan(),
                ]),
                Line::from(""),
            ];
            lines.extend(qr_lines(verification_uri));
            lines.push(Line::from(vec![
                crate::theme::key("[o] "),
                "open it in your browser · waiting for GitHub…".dim(),
            ]));
            lines.push(Line::from(vec![
                crate::theme::key("[t] "),
                "use a personal access token instead".dim(),
            ]));
            centered(frame, area, "Connect your GitHub account", lines);
        }
        other => {
            let mut lines = Vec::new();
            match other {
                Some(AuthState::Failed { reason }) => {
                    lines.push(Line::from(format!("The last attempt failed: {reason}")).red());
                }
                Some(AuthState::ReconnectRequired { login }) => {
                    lines.push(
                        Line::from(format!("GitHub no longer accepts the token for @{login}."))
                            .yellow(),
                    );
                }
                _ => {}
            }
            if let Some(error) = &app.login_error {
                lines.push(Line::from(error.clone()).red());
            }
            lines.push(Line::from(
                "PRowl reviews pull requests where you're a requested reviewer.",
            ));
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                crate::theme::key("[enter] "),
                "connect with a one-time code  ".into(),
                crate::theme::key("[t] "),
                "use a token  ".into(),
                crate::theme::key("[q] "),
                "quit".into(),
            ]));
            centered(frame, area, "Connect your GitHub account", lines);
        }
    }
}

fn policy_setup(frame: &mut Frame, area: Rect) {
    let lines = vec![
        Line::from("PRowl gives your Review Policy to Claude with every pull request it reviews."),
        Line::from("Rules under \"ALWAYS require human review\" decide which PRs wait for you."),
        Line::from(
            "A starter policy is in place; adapt it to your codebase now or any time later.",
        ),
        Line::from(""),
        Line::from(vec![
            crate::theme::key("[e] "),
            "edit it in your editor  ".into(),
            crate::theme::key("[enter] "),
            "continue".into(),
        ]),
    ];
    centered(frame, area, "Your Review Policy", lines);
}

fn qr_lines(text: &str) -> Vec<Line<'static>> {
    let Ok(code) = qrcode::QrCode::new(text.as_bytes()) else {
        return vec![];
    };
    code.render::<qrcode::render::unicode::Dense1x2>()
        .quiet_zone(true)
        .build()
        .lines()
        .map(|l| Line::from(l.to_owned()))
        .collect()
}

fn repo_picker(app: &App, frame: &mut Frame, area: Rect, onboarding: bool) {
    let [search, list, hints] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(area);
    let picker = &app.repos;
    let cursor = if picker.searching { "▏" } else { "" };
    frame.render_widget(
        Line::from(vec![
            " search: ".dim(),
            Span::raw(format!("{}{cursor}", picker.query)),
            if picker.loading {
                "  loading…".dim()
            } else {
                "".into()
            },
        ]),
        search,
    );
    match &picker.page {
        Some(page) if page.repos.is_empty() => {
            frame.render_widget(Paragraph::new("  No repositories match.").dim(), list);
        }
        Some(page) => {
            let items: Vec<ListItem> = page
                .repos
                .iter()
                .map(|r| {
                    let mark = if r.watched {
                        "[x] ".green()
                    } else {
                        "[ ] ".into()
                    };
                    let mut spans = vec![mark, Span::raw(r.full_name.clone())];
                    if r.private {
                        spans.push(" private".dim());
                    }
                    if r.archived {
                        spans.push(" archived".dim());
                    }
                    ListItem::new(Line::from(spans))
                })
                .collect();
            let pages = page.total.div_ceil(PER_PAGE).max(1);
            let mut state = ListState::default().with_selected(Some(picker.selected));
            frame.render_stateful_widget(
                List::new(items)
                    .block(
                        Block::default().borders(Borders::NONE).title_bottom(
                            Line::from(format!(
                                " {} repos · page {}/{} ",
                                page.total,
                                picker.page_no + 1,
                                pages
                            ))
                            .dim(),
                        ),
                    )
                    .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
                list,
                &mut state,
            );
        }
        None => frame.render_widget(Paragraph::new("  Loading repositories…").dim(), list),
    }
    let mut keys = vec![
        crate::theme::key("[/] "),
        "search  ".into(),
        crate::theme::key("[space] "),
        "watch/unwatch  ".into(),
        crate::theme::key("[n/p] "),
        "page  ".into(),
        crate::theme::key("[r] "),
        "refresh  ".into(),
    ];
    if onboarding {
        keys.extend([crate::theme::key("[enter] "), "done".into()]);
    }
    frame.render_widget(Line::from(keys), hints);
}

fn main_screen(app: &App, frame: &mut Frame, area: Rect) {
    let [tabs, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
    let titles = Tab::ALL.iter().enumerate().map(|(i, t)| {
        let mut spans = vec![
            Span::styled(format!("{} ", i + 1), Style::default().fg(theme::MUTED)),
            Span::raw(t.title()),
        ];
        // The queue tab shows how many PRs wait for you.
        if *t == Tab::Queue
            && let Some(n) = app.status.queue.filter(|n| *n > 0)
        {
            spans.push(Span::styled(
                format!(" {n}"),
                Style::default()
                    .fg(Color::Black)
                    .bg(theme::WARN)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        Line::from(spans)
    });
    frame.render_widget(
        Tabs::new(titles)
            .select(Tab::ALL.iter().position(|t| *t == app.tab))
            .divider(Span::styled("│", Style::default().fg(theme::MUTED)))
            .highlight_style(theme::accent().add_modifier(Modifier::UNDERLINED)),
        tabs,
    );
    let block = Block::bordered()
        .border_style(theme::border())
        .title(Span::styled(
            format!(" {} ", app.tab.title()),
            theme::accent(),
        ));
    let inner = block.inner(body);
    frame.render_widget(block, body);
    match app.tab {
        Tab::Repos => repo_picker(app, frame, inner, false),
        Tab::Policy => crate::policy_ui::draw(&app.policy, frame, inner),
        Tab::Feed => crate::feed_ui::draw_feed(&app.feed, frame, inner),
        Tab::Queue => crate::queue_ui::draw(&app.queue, frame, inner),
        Tab::Settings => crate::settings_ui::draw(&app.settings, frame, inner),
        Tab::History => crate::feed_ui::draw_history(&app.history, &app.status, frame, inner),
    }
}

fn status_bar(app: &App, frame: &mut Frame, area: Rect) {
    let s = &app.status;
    let bar = Style::default().bg(Color::Black);
    let sep = || Span::styled(" │ ", Style::default().fg(theme::MUTED));
    let mut spans: Vec<Span> = vec![match &s.account {
        Some(login) => Span::styled(format!(" ● @{login}"), Style::default().fg(theme::GOOD)),
        None => Span::styled(" ○ not connected", Style::default().fg(theme::BAD)),
    }];
    if let Some(n) = s.watched {
        spans.extend([
            sep(),
            Span::styled(format!("{n} watched"), Style::default().fg(theme::ACCENT)),
        ]);
    }
    if let Some(n) = s.queue {
        let text = format!("{n} awaiting you");
        spans.extend([
            sep(),
            if n > 0 {
                Span::styled(
                    text,
                    Style::default()
                        .fg(theme::WARN)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled(text, Style::default().fg(theme::MUTED))
            },
        ]);
    }
    if let Some(p) = &s.power {
        let mode = serde_json::to_value(p.mode)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default()
            .replace('_', " ");
        spans.extend([
            sep(),
            if p.holding {
                Span::styled(
                    format!("☀ awake ({mode})"),
                    Style::default().fg(theme::GOOD),
                )
            } else {
                Span::styled(
                    format!("☾ may sleep ({mode})"),
                    Style::default().fg(theme::MUTED),
                )
            },
        ]);
    }
    if let Some(c) = &s.costs {
        let (text, over) = match c.daily_budget_usd {
            Some(b) => (
                format!("${:.2} / ${b:.2} today", c.today_usd),
                c.today_usd >= b,
            ),
            None => (format!("${:.2} today", c.today_usd), false),
        };
        let color = if over { theme::BAD } else { theme::MONEY };
        spans.extend([sep(), Span::styled(text, Style::default().fg(color))]);
    }
    spans.push(Span::raw("   "));
    spans.extend(theme::hints("[?] help  [q] quit").spans);
    frame.render_widget(Paragraph::new(Line::from(spans)).style(bar), area);
}

fn help(frame: &mut Frame, area: Rect) {
    let lines = vec![
        Line::from(Span::styled("Keys", theme::accent())),
        Line::from(""),
        Line::from(vec![
            theme::key("  ← → or tab"),
            Span::raw("        next / previous tab (shift-tab goes back too)"),
        ]),
        Line::from(vec![
            theme::key("  1–6"),
            Span::raw("               jump to a tab"),
        ]),
        Line::from(vec![theme::key("  ↑↓ or j k"), Span::raw("         move")]),
        Line::from(vec![
            theme::key("  /"),
            Span::raw("                 search (esc to finish)"),
        ]),
        Line::from(vec![
            theme::key("  space"),
            Span::raw("             watch / unwatch a repository"),
        ]),
        Line::from("  Queue tab: enter open · a approve · r request changes · d dismiss · o open"),
        Line::from(vec![
            theme::key("  "),
            Span::raw("           in a PR: ↑↓ scroll diff · ] [ next/previous file · esc back"),
        ]),
        Line::from("  Feed tab: space pause/follow · f filter · c clear · p show polls"),
        Line::from("  History tab: enter details · o open on GitHub · f filter"),
        Line::from("  Policy tab: e edit · h history · r restore · n new override"),
        Line::from(vec![
            theme::key("  "),
            Span::raw("            x delete override · t dry-run a PR · esc back"),
        ]),
        Line::from(vec![
            theme::key("  n / p"),
            Span::raw("             next / previous page"),
        ]),
        Line::from(vec![
            theme::key("  r"),
            Span::raw("                 refresh or retry"),
        ]),
        Line::from(vec![
            theme::key("  ?"),
            Span::raw("                 this help"),
        ]),
        Line::from(vec![theme::key("  q, ctrl-c"), Span::raw("         quit")]),
        Line::from(""),
        Line::from("  press any key to close").dim(),
    ];
    let height = lines.len() as u16 + 2;
    let width = 52.min(area.width);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height: height.min(area.height),
    };
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().border_style(Style::default().fg(theme::ACCENT))),
        popup,
    );
}
