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
use tokio::sync::mpsc;

use crate::app::{App, Cmd, Msg, PER_PAGE, StatusBar};

pub struct Options {
    /// The daemon home, for installing the background service.
    pub home: PathBuf,
    /// How often the status bar refreshes.
    pub tick: Duration,
    /// If set, receives the screen as text after every draw (tests).
    pub snapshot: Option<Arc<Mutex<String>>>,
}

pub async fn run<B: Backend>(
    terminal: &mut Terminal<B>,
    client: ApiClient,
    mut keys: mpsc::UnboundedReceiver<KeyEvent>,
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
        if app.quit {
            break;
        }
        let msg = tokio::select! {
            key = keys.recv() => match key {
                Some(key) => Msg::Key(key),
                None => break,
            },
            Some(msg) = rx.recv() => msg,
            _ = tick.tick() => Msg::Tick,
        };
        for cmd in app.update(msg) {
            execute(cmd, &client, &tx, &options, &mut subscription);
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
    tokio::spawn(async move {
        let err = |e: prowld::client::ClientError| e.to_string();
        let msg = match cmd {
            Cmd::Connect => Msg::Connected(client.status().await.map_err(err)),
            Cmd::LoadDoctor => Msg::Doctor(client.doctor().await.map_err(err)),
            Cmd::LoadAuth => Msg::Auth(client.auth_state().await.map_err(err)),
            Cmd::StartLogin => Msg::Auth(client.start_login().await.map_err(err)),
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
            Cmd::Subscribe => unreachable!("handled above"),
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
    StatusBar {
        account: match status.ok().map(|s| s.github) {
            Some(AuthState::Connected { login, .. }) => Some(login),
            _ => None,
        },
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
