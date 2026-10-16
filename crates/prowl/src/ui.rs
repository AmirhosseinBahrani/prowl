//! Rendering: a pure function of [`App`].

use prowld::AuthState;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap};

use crate::app::{App, PER_PAGE, Screen, Tab};

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
            " PRowl ".bold().reversed(),
            " pull-request reviews, around the clock".dim(),
        ]),
        title,
    );
    match app.screen {
        Screen::Connecting => centered(frame, body, "Connecting to prowld…", vec![]),
        Screen::DaemonDown => daemon_down(app, frame, body),
        Screen::Doctor => doctor(app, frame, body),
        Screen::Login => login(app, frame, body),
        Screen::Repos => {
            let block = Block::bordered().title(" Choose repositories to watch ");
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
        "[i] ".bold(),
        "install and start it  ".into(),
        "[r] ".bold(),
        "retry  ".into(),
        "[q] ".bold(),
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
        "[r] ".bold(),
        "check again  ".into(),
        "[enter] ".bold(),
        "continue anyway  ".into(),
        "[q] ".bold(),
        "quit".into(),
    ]));
    centered(frame, area, "PRowl needs a couple of tools", lines);
}

fn login(app: &App, frame: &mut Frame, area: Rect) {
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
            lines.push(Line::from("Waiting for GitHub…").dim());
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
            lines.push(Line::from(
                "PRowl reviews pull requests where you're a requested reviewer.",
            ));
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                "[enter] ".bold(),
                "connect with a one-time code  ".into(),
                "[q] ".bold(),
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
            "[e] ".bold(),
            "edit it in your editor  ".into(),
            "[enter] ".bold(),
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
        "[/] ".bold(),
        "search  ".into(),
        "[space] ".bold(),
        "watch/unwatch  ".into(),
        "[n/p] ".bold(),
        "page  ".into(),
        "[r] ".bold(),
        "refresh  ".into(),
    ];
    if onboarding {
        keys.extend(["[enter] ".bold(), "done".into()]);
    }
    frame.render_widget(Line::from(keys), hints);
}

fn main_screen(app: &App, frame: &mut Frame, area: Rect) {
    let [tabs, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
    let titles = Tab::ALL
        .iter()
        .enumerate()
        .map(|(i, t)| format!("{} {}", i + 1, t.title()));
    frame.render_widget(
        Tabs::new(titles)
            .select(Tab::ALL.iter().position(|t| *t == app.tab))
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        tabs,
    );
    let block = Block::bordered().title(format!(" {} ", app.tab.title()));
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
    let mut spans: Vec<Span> = Vec::new();
    let sep = || " · ".dim();
    spans.push(match &s.account {
        Some(login) => Span::raw(format!(" @{login}")),
        None => " not connected".dim(),
    });
    if let Some(n) = s.watched {
        spans.extend([sep(), Span::raw(format!("{n} watched"))]);
    }
    if let Some(n) = s.queue {
        let text = format!("{n} awaiting you");
        spans.extend([
            sep(),
            if n > 0 {
                text.yellow().bold()
            } else {
                Span::raw(text)
            },
        ]);
    }
    if let Some(p) = &s.power {
        let mode = serde_json::to_value(p.mode)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        spans.extend([
            sep(),
            Span::raw(if p.holding {
                format!("awake ({mode})")
            } else {
                format!("may sleep ({mode})")
            }),
        ]);
    }
    if let Some(c) = &s.costs {
        let text = match c.daily_budget_usd {
            Some(b) => format!("${:.2} / ${b:.2} today", c.today_usd),
            None => format!("${:.2} today", c.today_usd),
        };
        spans.extend([sep(), Span::raw(text)]);
    }
    spans.extend(["   ? help  q quit".dim()]);
    frame.render_widget(Line::from(spans).reversed(), area);
}

fn help(frame: &mut Frame, area: Rect) {
    let lines = vec![
        Line::from("Keys").bold(),
        Line::from(""),
        Line::from("  tab / shift-tab   next / previous tab"),
        Line::from("  1–6               jump to a tab"),
        Line::from("  ↑↓ or j k         move"),
        Line::from("  /                 search (esc to finish)"),
        Line::from("  space             watch / unwatch a repository"),
        Line::from("  Queue tab: enter open · a approve · r request changes · d dismiss · o open"),
        Line::from("             in a PR: ↑↓ scroll diff · ] [ next/previous file · esc back"),
        Line::from("  Feed tab: space pause/follow · f filter · c clear · p show polls"),
        Line::from("  History tab: enter details · o open on GitHub · f filter"),
        Line::from("  Policy tab: e edit · h history · r restore · n new override"),
        Line::from("              x delete override · t dry-run a PR · esc back"),
        Line::from("  n / p             next / previous page"),
        Line::from("  r                 refresh or retry"),
        Line::from("  ?                 this help"),
        Line::from("  q, ctrl-c         quit"),
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
    frame.render_widget(Paragraph::new(lines).block(Block::bordered()), popup);
}
