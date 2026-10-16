//! The real `prowl` TUI on a test terminal, driven by key presses against a real daemon.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::{Method, StatusCode};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use prowl_e2e::Harness;
use prowld::{ApiClient, RepoQuery};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::json;
use tokio::sync::mpsc;

struct Tui {
    keys: mpsc::UnboundedSender<KeyEvent>,
    screen: Arc<Mutex<String>>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Tui {
    fn start(client: ApiClient, home: std::path::PathBuf) -> Self {
        let (keys, rx) = mpsc::unbounded_channel();
        let screen = Arc::new(Mutex::new(String::new()));
        let snapshot = screen.clone();
        let task = tokio::spawn(async move {
            let mut terminal = Terminal::new(TestBackend::new(100, 34)).unwrap();
            prowl::run(
                &mut terminal,
                client,
                rx,
                prowl::Options {
                    home,
                    tick: Duration::from_millis(100),
                    snapshot: Some(snapshot),
                    editor: prowl::Editor::Hook(std::sync::Arc::new(|_| {})),
                    input_paused: None,
                },
            )
            .await
        });
        Self { keys, screen, task }
    }

    fn of(h: &Harness) -> Self {
        Self::start(h.client(), h.daemon().home().to_path_buf())
    }

    fn screen(&self) -> String {
        self.screen.lock().unwrap().clone()
    }

    async fn wait_for(&self, text: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !self.screen().contains(text) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "screen never showed {text:?}:\n{}",
                self.screen()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn press(&self, code: KeyCode) {
        self.keys
            .send(KeyEvent::new(code, KeyModifiers::NONE))
            .unwrap();
    }

    fn type_text(&self, text: &str) {
        for c in text.chars() {
            self.press(KeyCode::Char(c));
        }
    }

    async fn quit(self) {
        self.press(KeyCode::Char('q'));
        tokio::time::timeout(Duration::from_secs(5), self.task)
            .await
            .expect("TUI quits")
            .unwrap()
            .unwrap();
    }
}

fn repos(h: &Harness) {
    let repo = |id: i64, name: &str| {
        let (owner, short) = name.split_once('/').unwrap();
        json!({
            "id": id, "full_name": name, "name": short, "owner": { "login": owner },
            "private": id == 2, "archived": false, "default_branch": "main",
            "html_url": format!("https://github.com/{name}")
        })
    };
    h.github.route(
        Method::GET,
        "/user/repos",
        StatusCode::OK,
        json!([
            repo(1, "acme/api"),
            repo(2, "acme/payments"),
            repo(3, "octo/dotfiles")
        ]),
    );
}

#[tokio::test]
async fn without_a_daemon_it_explains_and_offers_to_install() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("nobody-home");
    let client = ApiClient::new(prowld::transport::Endpoint::for_home(&home), "x");

    let tui = Tui::start(client, home);

    tui.wait_for("prowld is not running").await;
    tui.wait_for("[i] install and start it").await;
    tui.quit().await;
}

#[tokio::test]
async fn first_run_connects_github_and_picks_repositories() {
    let h = Harness::start().await.unwrap();
    repos(&h);
    h.github.route(
        Method::POST,
        "/login/device/code",
        StatusCode::OK,
        json!({
            "device_code": "dev", "user_code": "WXYZ-1234",
            "verification_uri": "https://github.com/login/device", "expires_in": 900, "interval": 0
        }),
    );
    h.github.route_seq(
        Method::POST,
        "/login/oauth/access_token",
        vec![
            (StatusCode::OK, json!({ "error": "authorization_pending" })),
            (
                StatusCode::OK,
                json!({ "access_token": "gho_t", "token_type": "bearer" }),
            ),
        ],
    );
    h.github.route(
        Method::GET,
        "/user",
        StatusCode::OK,
        json!({ "login": "octo", "name": null }),
    );
    let tui = Tui::of(&h);

    tui.wait_for("Connect your GitHub account").await;
    tui.press(KeyCode::Enter);
    tui.wait_for("WXYZ-1234").await;
    assert!(tui.screen().contains("https://github.com/login/device"));
    assert!(
        tui.screen().contains('▀') || tui.screen().contains('▄'),
        "QR code drawn"
    );

    // The daemon finishes the device flow; the TUI moves on to the picker by itself.
    tui.wait_for("Choose repositories to watch").await;
    tui.wait_for("acme/payments").await;
    tui.press(KeyCode::Enter);
    tui.wait_for("Watch at least one repository").await;

    tui.press(KeyCode::Char('/'));
    tui.type_text("pay");
    tui.press(KeyCode::Esc);
    tui.wait_for("1 repos").await;
    assert!(!tui.screen().contains("acme/api"), "{}", tui.screen());
    tui.press(KeyCode::Char(' '));
    tui.wait_for("[x] acme/payments").await;
    let watched: Vec<_> = h
        .client()
        .watched_repos()
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.full_name)
        .collect();
    assert_eq!(watched, ["acme/payments"]);

    tui.press(KeyCode::Enter);
    tui.wait_for("Your Review Policy").await;
    tui.press(KeyCode::Enter);
    tui.wait_for("1 Queue").await;
    tui.wait_for("@octo").await;
    tui.wait_for("1 watched").await;
    tui.quit().await;
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_set_up_install_opens_straight_on_the_main_screen() {
    let h = Harness::start().await.unwrap();
    h.connect_github("octo").await.unwrap();
    repos(&h);
    h.client().repos(&RepoQuery::default()).await.unwrap();
    h.client().watch("acme/api").await.unwrap();

    let tui = Tui::of(&h);

    tui.wait_for("1 Queue").await;
    tui.wait_for("0 awaiting you").await;
    tui.press(KeyCode::Char('5'));
    tui.wait_for("[x] acme/api").await;
    tui.press(KeyCode::Char('?'));
    tui.wait_for("watch / unwatch a repository").await;
    tui.press(KeyCode::Char('x'));
    tui.quit().await;
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn missing_tools_are_explained_before_anything_else() {
    let h = Harness::start().await.unwrap();
    h.claude.set_logged_in(false).unwrap();

    let tui = Tui::of(&h);

    tui.wait_for("PRowl needs a couple of tools").await;
    tui.wait_for("claude auth login").await;
    tui.press(KeyCode::Enter);
    tui.wait_for("Connect your GitHub account").await;
    tui.quit().await;
    h.shutdown().await.unwrap();
}
