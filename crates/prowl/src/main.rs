use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};
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
    #[arg(long, env = "PROWL_HOME", global = true)]
    home: Option<PathBuf>,

    /// Without a command, prowl opens the terminal UI.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// List the pull requests waiting for your review.
    Queue,
    /// Approve a pull request on GitHub (as you) and remove it from the queue.
    Approve {
        /// owner/repo#number
        pr: String,
        #[arg(short, long, default_value = "")]
        message: String,
    },
    /// Request changes on a pull request (as you) and remove it from the queue.
    RequestChanges {
        /// owner/repo#number
        pr: String,
        #[arg(short, long)]
        message: String,
    },
    /// Remove a pull request from the queue without doing anything on GitHub.
    Dismiss {
        /// owner/repo#number
        pr: String,
    },
    /// Review a pull request again at its current commit.
    Rerun {
        /// owner/repo#number
        pr: String,
    },
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

    if let Some(command) = cli.command {
        use prowl::queue_tab::QueueAction;
        let output = match command {
            Command::Queue => prowl::cli::queue(&client).await?,
            Command::Approve { pr, message } => {
                prowl::cli::resolve(&client, &pr, QueueAction::Approve, &message).await?
            }
            Command::RequestChanges { pr, message } => {
                prowl::cli::resolve(&client, &pr, QueueAction::RequestChanges, &message).await?
            }
            Command::Dismiss { pr } => {
                prowl::cli::resolve(&client, &pr, QueueAction::Dismiss, "").await?
            }
            Command::Rerun { pr } => prowl::cli::rerun(&client, &pr).await?,
        };
        print!("{output}");
        return Ok(());
    }
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
                            if keys_tx.send(prowl::Input::Key(key)).is_err() {
                                return;
                            }
                        }
                        Some(Ok(Event::Resize(_, _))) => {
                            if keys_tx.send(prowl::Input::Resize).is_err() {
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
            after_draw: Some(Box::new(|buffer| {
                let links = prowl::links::find_links(buffer);
                if !links.is_empty() {
                    let _ = prowl::links::write_hyperlinks(&mut std::io::stdout(), &links);
                }
            })),
            editor: prowl::Editor::System,
            opener: prowl::Opener::System,
            input_paused: Some(paused_tx),
        },
    )
    .await;
    ratatui::restore();
    result
}
