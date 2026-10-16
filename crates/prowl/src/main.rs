use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use crossterm::event::{Event, EventStream};
use futures::StreamExt;
use prowld::ApiClient;
use prowld::transport::Endpoint;

#[derive(Parser)]
#[command(
    name = "prowl",
    version,
    about = "PRowl: pull-request reviews, around the clock"
)]
struct Cli {
    /// PRowl home of the daemon to talk to.
    #[arg(long, env = "PROWL_HOME")]
    home: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let home = match cli.home {
        Some(home) => home,
        None => prowld::config::default_home()?,
    };
    // Without a token yet (daemon never started), connect anyway: the UI explains and offers
    // to install the service.
    let token = prowld::config::read_token(&home).unwrap_or_default();
    let client = ApiClient::new(Endpoint::for_home(&home), token);

    let (keys_tx, keys) = tokio::sync::mpsc::unbounded_channel();
    // While an editor owns the terminal, stop reading keys so it gets them all.
    let (paused_tx, mut paused) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        loop {
            while *paused.borrow() {
                if paused.changed().await.is_err() {
                    return;
                }
            }
            let mut events = EventStream::new();
            loop {
                tokio::select! {
                    event = events.next() => match event {
                        Some(Ok(Event::Key(key))) => {
                            if keys_tx.send(key).is_err() {
                                return;
                            }
                        }
                        Some(Ok(_)) => {}
                        _ => return,
                    },
                    changed = paused.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        if *paused.borrow() {
                            break;
                        }
                    }
                }
            }
        }
    });

    let mut terminal = ratatui::init();
    let result = prowl::run(
        &mut terminal,
        client,
        keys,
        prowl::Options {
            home,
            tick: Duration::from_secs(5),
            snapshot: None,
            editor: prowl::Editor::System,
            input_paused: Some(paused_tx),
        },
    )
    .await;
    ratatui::restore();
    result
}
