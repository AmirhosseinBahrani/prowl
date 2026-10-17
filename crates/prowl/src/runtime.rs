//! Runs the app: draws, feeds it keys, daemon events and ticks, and carries out its commands.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::KeyEvent;
use futures::StreamExt;
use prowld::{ApiClient, AuthState, RepoQuery};
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use tokio::sync::mpsc;

use crate::app::{App, Cmd, Msg, PER_PAGE, StatusBar};

/// How policy files are edited.
#[derive(Clone)]
pub enum Editor {
    /// Suspend the UI and run `$VISUAL` / `$EDITOR` in the terminal.
    System,
    /// Call this instead (tests).
    Hook(Arc<dyn Fn(&std::path::Path) + Send + Sync>),
}

/// How URLs are opened.
#[derive(Clone)]
pub enum Opener {
    /// The OS default browser.
    System,
    Hook(Arc<dyn Fn(&str) + Send + Sync>),
}

/// A hook run with the screen after every draw.
pub type AfterDraw = Box<dyn Fn(&Buffer) + Send + Sync>;

/// What the terminal sends the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    Key(KeyEvent),
    /// The window changed size: redraw to fit.
    Resize,
}

pub struct Options {
    /// The daemon home, for installing the background service.
    pub home: PathBuf,
    /// How often the status bar refreshes.
    pub tick: Duration,
    /// If set, receives the screen as text after every draw (tests).
    pub snapshot: Option<Arc<Mutex<String>>>,
    /// Called with the screen after every draw (e.g. to add clickable links).
    pub after_draw: Option<AfterDraw>,
    pub editor: Editor,
    pub opener: Opener,
    /// Pauses the key reader while an editor owns the terminal.
    pub input_paused: Option<tokio::sync::watch::Sender<bool>>,
}

pub async fn run<B: Backend>(
    terminal: &mut Terminal<B>,
    client: ApiClient,
    mut keys: mpsc::UnboundedReceiver<Input>,
    options: Options,
) -> anyhow::Result<()>
where
    B::Error: Send + Sync + 'static,
{
    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    let mut app = App::new(client.endpoint().to_string());
    let mut tick = tokio::time::interval(options.tick);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut subscription: Option<tokio::task::JoinHandle<()>> = None;

    for cmd in app.init() {
        execute(cmd, &client, &tx, &options, &mut subscription);
    }
    loop {
        let completed = terminal.draw(|frame| crate::ui::draw(&app, frame))?;
        if let Some(snapshot) = &options.snapshot {
            *snapshot.lock().unwrap() = buffer_text(completed.buffer);
        }
        if let Some(after_draw) = &options.after_draw {
            after_draw(completed.buffer);
        }
        if app.quit {
            break;
        }
        let msg = tokio::select! {
            input = keys.recv() => match input {
                Some(Input::Key(key)) => Msg::Key(key),
                Some(Input::Resize) => {
                    // Links are re-written per draw, so start the new size from a clean screen.
                    redraw_from_scratch(terminal)?;
                    continue;
                }
                None => break,
            },
            Some(msg) = rx.recv() => msg,
            _ = tick.tick() => Msg::Tick,
        };
        for cmd in app.update(msg) {
            match cmd {
                // These need the terminal, so they run here rather than in a task.
                Cmd::EditPolicy { scope } => {
                    let result = edit_policy(terminal, &client, &options, &scope).await;
                    let _ = tx.send(Msg::Edited(result));
                }
                Cmd::CreateOverride { repo } => {
                    let result = create_override(terminal, &client, &options, &repo).await;
                    let _ = tx.send(Msg::PolicyChanged(result));
                }
                cmd => execute(cmd, &client, &tx, &options, &mut subscription),
            }
        }
    }
    if let Some(task) = subscription {
        task.abort();
    }
    Ok(())
}

fn execute(
    cmd: Cmd,
    client: &ApiClient,
    tx: &mpsc::UnboundedSender<Msg>,
    options: &Options,
    subscription: &mut Option<tokio::task::JoinHandle<()>>,
) {
    let client = client.clone();
    let tx = tx.clone();
    if cmd == Cmd::Subscribe {
        if subscription.is_none() {
            *subscription = Some(tokio::spawn(subscribe(client, tx)));
        }
        return;
    }
    let home = options.home.clone();
    let opener = options.opener.clone();
    tokio::spawn(async move {
        let err = |e: prowld::client::ClientError| e.to_string();
        let msg = match cmd {
            Cmd::Connect => Msg::Connected(client.status().await.map_err(err)),
            Cmd::LoadDoctor => Msg::Doctor(client.doctor().await.map_err(err)),
            Cmd::LoadAuth => Msg::Auth(client.auth_state().await.map_err(err)),
            Cmd::StartLogin => Msg::SignIn(client.start_login().await.map_err(err)),
            Cmd::SignOut => Msg::Auth(client.disconnect().await.map_err(err)),
            Cmd::SignInWithToken { token } => {
                Msg::SignIn(client.sign_in_with_token(&token).await.map_err(err))
            }
            Cmd::LoadWatchedCount => {
                Msg::WatchedCount(client.watched_repos().await.map(|r| r.len()).map_err(err))
            }
            Cmd::LoadRepos {
                request,
                query,
                page,
                refresh,
            } => {
                let query = RepoQuery {
                    q: (!query.is_empty()).then_some(query),
                    page: Some(page),
                    per_page: Some(PER_PAGE),
                    refresh,
                };
                Msg::Repos(request, client.repos(&query).await.map_err(err))
            }
            Cmd::SetWatched { full_name, watch } => {
                let result = if watch {
                    client.watch(&full_name).await
                } else {
                    client.unwatch(&full_name).await
                };
                Msg::WatchToggled(result.map_err(err))
            }
            Cmd::LoadStatusBar => Msg::StatusBar(status_bar(&client).await),
            Cmd::InstallService => Msg::ServiceInstalled(install_service(home).await),
            Cmd::LoadPolicies => Msg::Policies(client.policies().await.map_err(err)),
            Cmd::LoadPolicy { scope } => match parse_scope(&scope) {
                Ok(parsed) => match client.policy(&parsed).await {
                    Err(prowld::client::ClientError::Status { status, .. })
                        if status == 404 && scope != "global" =>
                    {
                        Msg::PolicyGone(scope)
                    }
                    result => Msg::Policy(result.map_err(err)),
                },
                Err(e) => Msg::Policy(Err(e)),
            },
            Cmd::LoadVersions { scope } => match parse_scope(&scope) {
                Ok(scope) => Msg::Versions(client.policy_versions(&scope).await.map_err(err)),
                Err(e) => Msg::Versions(Err(e)),
            },
            Cmd::DeleteOverride { scope } => Msg::PolicyChanged(match parse_scope(&scope) {
                Ok(s) => client
                    .delete_policy(&s)
                    .await
                    .map(|()| format!("Deleted the override for {scope}."))
                    .map_err(err),
                Err(e) => Err(e),
            }),
            Cmd::RestorePolicy { scope, version_id } => {
                Msg::PolicyChanged(match parse_scope(&scope) {
                    Ok(s) => client
                        .restore_policy(&s, version_id)
                        .await
                        .map(|p| format!("Restored v{version_id} as v{}.", p.version_id))
                        .map_err(err),
                    Err(e) => Err(e),
                })
            }
            Cmd::DryRun { repo, pr } => {
                Msg::DryRunDone(client.dry_run(&repo, pr).await.map_err(err))
            }
            Cmd::LoadRecentEvents => {
                Msg::RecentEvents(client.recent_events(500).await.map_err(err))
            }
            Cmd::LoadReviews => Msg::Reviews(client.reviews(None).await.map_err(err)),
            Cmd::LoadJobs => Msg::Jobs(client.jobs().await.map_err(err)),
            Cmd::LoadTranscript { job_id } => {
                Msg::Transcript(job_id, client.transcript(job_id).await.map_err(err))
            }
            Cmd::LoadQueue => Msg::QueueItems(client.queue().await.map_err(err)),
            Cmd::LoadSettings => Msg::Settings(client.settings().await.map_err(err)),
            Cmd::SaveSettings { patch } => {
                Msg::SettingsSaved(client.update_settings(patch).await.map_err(err))
            }
            Cmd::LoadQueueItem { repo, number } => {
                Msg::QueueDetail(client.queue_item(&repo, number).await.map_err(err))
            }
            Cmd::Resolve {
                repo,
                number,
                action,
                comment,
            } => {
                use crate::queue_tab::QueueAction;
                let result = match action {
                    QueueAction::Approve => client.approve(&repo, number, &comment).await,
                    QueueAction::RequestChanges => {
                        client.request_changes(&repo, number, &comment).await
                    }
                    QueueAction::Dismiss => client.dismiss(&repo, number).await,
                };
                Msg::Resolved {
                    repo,
                    number,
                    action,
                    result: result.map_err(err),
                }
            }
            Cmd::OpenUrl { url } => Msg::Opened(open_url(&opener, &url)),
            Cmd::Subscribe | Cmd::EditPolicy { .. } | Cmd::CreateOverride { .. } => {
                unreachable!("handled by the run loop")
            }
        };
        let _ = tx.send(msg);
    });
}

/// Streams daemon events into the app, reconnecting (from the last event seen) if the
/// connection drops, e.g. while the daemon restarts.
async fn subscribe(client: ApiClient, tx: mpsc::UnboundedSender<Msg>) {
    // Only events from now on matter to the UI; after a reconnect, resume from the last seen.
    let mut last: Option<i64> = None;
    loop {
        let stream = match last {
            None => client.live_events().await.map(|s| s.boxed()),
            Some(id) => client.events(id).await.map(|s| s.boxed()),
        };
        if let Ok(mut stream) = stream {
            while let Some(Ok(event)) = stream.next().await {
                last = Some(event.id);
                if tx.send(Msg::Event(event)).is_err() {
                    return;
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn status_bar(client: &ApiClient) -> StatusBar {
    let (status, watched, queue, power, costs) = tokio::join!(
        client.status(),
        client.watched_repos(),
        client.queue(),
        client.power(),
        client.costs(),
    );
    let (account, update) = match status {
        Ok(status) => (
            match status.github {
                AuthState::Connected { login, .. } => Some(login),
                _ => None,
            },
            status.update,
        ),
        Err(_) => (None, None),
    };
    StatusBar {
        account,
        update,
        watched: watched.ok().map(|r| r.len()),
        queue: queue.ok().map(|q| q.len()),
        power: power.ok(),
        costs: costs.ok(),
    }
}

async fn install_service(home: PathBuf) -> Result<String, String> {
    tokio::task::spawn_blocking(move || {
        use prowld::service::{DEFAULT_NAME, ServiceSpec, install};
        let spec =
            ServiceSpec::beside_current_exe(DEFAULT_NAME, &home).map_err(|e| format!("{e:#}"))?;
        let status = install(&spec).map_err(|e| format!("{e:#}"))?;
        Ok(if status.running {
            "Background service installed and running.".to_owned()
        } else {
            "Background service installed; waiting for it to start…".to_owned()
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

fn open_url(opener: &Opener, url: &str) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(format!("not opening {url}: not a web address"));
    }
    match opener {
        Opener::Hook(hook) => {
            hook(url);
            Ok(())
        }
        Opener::System => {
            let mut cmd = if cfg!(target_os = "macos") {
                std::process::Command::new("open")
            } else if cfg!(windows) {
                let mut c = std::process::Command::new("rundll32");
                c.arg("url.dll,FileProtocolHandler");
                c
            } else {
                std::process::Command::new("xdg-open")
            };
            cmd.arg(url)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .map(drop)
                .map_err(|e| format!("could not open a browser: {e}"))
        }
    }
}

fn parse_scope(scope: &str) -> Result<prowld::policy::Scope, String> {
    if scope == "global" {
        Ok(prowld::policy::Scope::Global)
    } else {
        prowld::policy::Scope::parse_repo(scope).map_err(|e| e.to_string())
    }
}

/// Clears the screen and makes the next draw repaint everything. Unlike `Terminal::clear`, this
/// never asks the terminal for the cursor position, which some terminals answer slowly or not
/// at all ("The cursor position could not be read within a normal duration").
fn redraw_from_scratch<B: Backend>(terminal: &mut Terminal<B>) -> Result<(), B::Error> {
    let size = terminal.size()?;
    terminal.resize(Rect::new(0, 0, size.width, size.height))
}

async fn create_override<B: Backend>(
    terminal: &mut Terminal<B>,
    client: &ApiClient,
    options: &Options,
    repo: &str,
) -> Result<String, String>
where
    B::Error: Send + Sync + 'static,
{
    let scope = prowld::policy::Scope::parse_repo(repo).map_err(|e| e.to_string())?;
    let global = client
        .policy(&prowld::policy::Scope::Global)
        .await
        .map_err(|e| e.to_string())?;
    client
        .save_policy(&scope, &global.content)
        .await
        .map_err(|e| e.to_string())?;
    edit_policy(terminal, client, options, repo).await?;
    Ok(format!(
        "Created an override for {repo}, starting from the global policy."
    ))
}

async fn edit_policy<B: Backend>(
    terminal: &mut Terminal<B>,
    client: &ApiClient,
    options: &Options,
    scope: &str,
) -> Result<(), String>
where
    B::Error: Send + Sync + 'static,
{
    let list = client.policies().await.map_err(|e| e.to_string())?;
    let path = list
        .into_iter()
        .find(|p| p.scope == scope)
        .map(|p| PathBuf::from(p.path))
        .ok_or_else(|| format!("no policy file for {scope}"))?;
    match &options.editor {
        Editor::Hook(hook) => hook(&path),
        Editor::System => {
            if let Some(paused) = &options.input_paused {
                let _ = paused.send(true);
            }
            let result = run_system_editor(&path);
            if let Some(paused) = &options.input_paused {
                let _ = paused.send(false);
            }
            redraw_from_scratch(terminal).map_err(|e| e.to_string())?;
            result?;
        }
    }
    // Reading the policy makes the daemon pick up the edit straight away.
    client
        .policy(&parse_scope(scope)?)
        .await
        .map(drop)
        .map_err(|e| e.to_string())
}

/// Hands the terminal to `$VISUAL` / `$EDITOR` (falling back to a common default) and takes
/// it back afterwards.
fn run_system_editor(path: &std::path::Path) -> Result<(), String> {
    use crossterm::execute;
    use crossterm::terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    };

    let configured = std::env::var("VISUAL")
        .ok()
        .or_else(|| std::env::var("EDITOR").ok())
        .filter(|e| !e.trim().is_empty());
    let command = configured.unwrap_or_else(|| {
        if cfg!(windows) {
            "notepad".into()
        } else if which_exists("nano") {
            "nano".into()
        } else {
            "vi".into()
        }
    });
    let mut parts = command.split_whitespace();
    let program = parts.next().ok_or("no editor configured")?;
    let args: Vec<&str> = parts.collect();

    let mut stdout = std::io::stdout();
    let _ = disable_raw_mode();
    let _ = execute!(stdout, LeaveAlternateScreen, crossterm::cursor::Show);
    let status = std::process::Command::new(program)
        .args(&args)
        .arg(path)
        .status();
    let _ = execute!(stdout, EnterAlternateScreen, crossterm::cursor::Hide);
    let _ = enable_raw_mode();
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("{program} exited with {s}")),
        Err(e) => Err(format!("could not start {program}: {e} (set $EDITOR)")),
    }
}

fn which_exists(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

/// The screen as plain text, one line per row.
pub fn buffer_text(buffer: &Buffer) -> String {
    let area = buffer.area;
    let mut out = String::new();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}
