//! The real `prowl` TUI on a test terminal, driven by key presses against a real daemon.

mod common;

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

/// URLs the TUI asked to open, across tests (each test checks for its own).
static OPENED: Mutex<Vec<String>> = Mutex::new(Vec::new());

struct Tui {
    keys: mpsc::UnboundedSender<KeyEvent>,
    screen: Arc<Mutex<String>>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Tui {
    fn start(client: ApiClient, home: std::path::PathBuf) -> Self {
        Self::start_with_editor(client, home, Arc::new(|_| {}))
    }

    fn start_with_editor(
        client: ApiClient,
        home: std::path::PathBuf,
        editor: Arc<dyn Fn(&std::path::Path) + Send + Sync>,
    ) -> Self {
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
                    editor: prowl::Editor::Hook(editor),
                    opener: prowl::Opener::Hook(Arc::new(|url: &str| {
                        OPENED.lock().unwrap().push(url.to_owned())
                    })),
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

    /// With an "editor" that records which file it opened and appends `addition` to it.
    fn with_editor(
        h: &Harness,
        addition: &'static str,
    ) -> (Self, Arc<Mutex<Vec<std::path::PathBuf>>>) {
        let opened = Arc::new(Mutex::new(Vec::new()));
        let log = opened.clone();
        let editor = Arc::new(move |path: &std::path::Path| {
            log.lock().unwrap().push(path.to_path_buf());
            // Like a real editor, keep whatever line endings the file has.
            let mut text = std::fs::read_to_string(path).unwrap().replace("\r\n", "\n");
            text = text.replace(
                "## ALWAYS require human review\n",
                &format!("## ALWAYS require human review\n{addition}"),
            );
            std::fs::write(path, text).unwrap();
        });
        (
            Self::start_with_editor(h.client(), h.daemon().home().to_path_buf(), editor),
            opened,
        )
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
    let (tui, opened) = Tui::with_editor(&h, "- [CRYPTO] Cryptography\n");

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
    tui.press(KeyCode::Char('e'));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while opened.lock().unwrap().is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "editor never opened"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(opened.lock().unwrap()[0].ends_with("global.md"));
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

/// Connected, watching acme/api, on the main screen's Policy tab.
async fn on_policy_tab(
    addition: &'static str,
) -> (Harness, Tui, Arc<Mutex<Vec<std::path::PathBuf>>>) {
    let h = Harness::start().await.unwrap();
    h.connect_github("octo").await.unwrap();
    repos(&h);
    h.client().repos(&RepoQuery::default()).await.unwrap();
    h.client().watch("acme/api").await.unwrap();
    let (tui, opened) = Tui::with_editor(&h, addition);
    tui.wait_for("1 Queue").await;
    tui.press(KeyCode::Char('4'));
    tui.wait_for("global · version").await;
    (h, tui, opened)
}

#[tokio::test]
async fn the_policy_tab_edits_in_the_editor_and_shows_the_new_version() {
    let (h, tui, opened) = on_policy_tab("- [CRYPTO] Cryptography\n").await;
    assert!(tui.screen().contains("6 rules"));
    assert!(tui.screen().contains("[AUTH]"));

    tui.press(KeyCode::Char('e'));

    tui.wait_for("[CRYPTO] Cryptography").await;
    tui.wait_for("7 rules").await;
    assert!(opened.lock().unwrap()[0].ends_with("global.md"));
    let latest = &h
        .client()
        .policy_versions(&prowld::policy::Scope::Global)
        .await
        .unwrap()[0];
    assert_eq!(latest.source, "file");
    tui.quit().await;
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn history_shows_the_diff_and_restores_a_version() {
    let (h, tui, _) = on_policy_tab("- [CRYPTO] Cryptography\n").await;
    tui.press(KeyCode::Char('e'));
    tui.wait_for("7 rules").await;

    tui.press(KeyCode::Char('h'));
    tui.wait_for("versions (newest first)").await;
    tui.wait_for("default").await;
    tui.press(KeyCode::Down);
    tui.wait_for("+ - [CRYPTO] Cryptography").await;
    tui.press(KeyCode::Char('r'));
    tui.wait_for("Restore version").await;
    tui.press(KeyCode::Char('y'));
    tui.wait_for("Restored v").await;
    tui.press(KeyCode::Esc);
    tui.wait_for("6 rules").await;
    // The list and the reloaded content arrive separately; wait for the content too.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tui.screen().contains("[CRYPTO]") {
        assert!(tokio::time::Instant::now() < deadline, "{}", tui.screen());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tui.quit().await;
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn overrides_are_created_in_the_editor_and_deleted_after_confirming() {
    let (h, tui, opened) = on_policy_tab("- [API] Public API changes\n").await;

    tui.press(KeyCode::Char('n'));
    tui.wait_for("override for repository").await;
    tui.type_text("acme/api");
    tui.press(KeyCode::Enter);
    tui.wait_for("Created an override for acme/api").await;
    tui.wait_for("acme/api  7 rules").await;
    assert!(
        opened
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .ends_with("acme/api.md")
    );
    let effective = h.client().effective_policy(Some("acme/api")).await.unwrap();
    assert!(effective.content.contains("[API] Public API changes"));

    tui.press(KeyCode::Down);
    tui.press(KeyCode::Char('x'));
    tui.wait_for("Delete the override for acme/api?").await;
    tui.press(KeyCode::Char('y'));
    tui.wait_for("Deleted the override for acme/api").await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tui.screen().contains("acme/api  7 rules") {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        h.client()
            .effective_policy(Some("acme/api"))
            .await
            .unwrap()
            .scope,
        "global"
    );
    tui.quit().await;
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_dry_run_shows_what_would_happen_without_posting() {
    let (h, tui, _) = on_policy_tab("").await;
    let commits = h
        .git
        .create_pr(
            "acme/api",
            7,
            &[("README.md", "# api\n")],
            &[("src/auth.rs", "fn auth() {}\n")],
        )
        .unwrap();
    h.github.route(
        Method::GET,
        "/repos/acme/api/pulls/7",
        StatusCode::OK,
        json!({
            "number": 7, "state": "open", "draft": false, "title": "Auth change",
            "html_url": "https://github.com/acme/api/pull/7", "user": { "login": "alice" },
            "body": "", "head": { "sha": commits.head_sha, "ref": "f" },
            "base": { "sha": commits.base_sha, "ref": "main" },
            "additions": 1, "deletions": 0, "changed_files": 1, "labels": [],
            "requested_reviewers": [], "requested_teams": []
        }),
    );
    h.claude
        .respond_with(vec![prowl_e2e::ClaudeResponse::review(&json!({
            "summary": "Changes auth.", "quick_notes": ["Touches login"], "risk_score": 70,
            "severity": "high", "touches_critical_sections": true,
            "matched_policy_rules": [{ "id": "AUTH", "evidence": "src/auth.rs:1" }],
            "findings": [], "recommendation": "request_changes"
        }))])
        .unwrap();
    let writes = h.github.writes().len();

    tui.press(KeyCode::Char('t'));
    tui.type_text("acme/api#7");
    tui.press(KeyCode::Enter);

    tui.wait_for("this PR would wait for a human review").await;
    assert!(tui.screen().contains("matched [AUTH] src/auth.rs:1"));
    assert!(tui.screen().contains("Summary: Changes auth."));
    assert_eq!(h.github.writes().len(), writes, "a dry run posts nothing");
    tui.press(KeyCode::Esc);
    tui.wait_for("global · version").await;
    tui.quit().await;
    h.shutdown().await.unwrap();
}

/// A reviewed PR (#7, posted by the agent) and the TUI on the main screen.
async fn after_a_review() -> (Harness, Tui) {
    let (h, _) = common::setup(&[7]).await;
    h.claude
        .respond_with(vec![prowl_e2e::ClaudeResponse::review(
            &common::calm_review(),
        )])
        .unwrap();
    let tui = Tui::of(&h);
    tui.wait_for("1 Queue").await;
    h.client().poll_now().await.unwrap();
    common::wait_for_job(&h, common::finished("done")).await;
    (h, tui)
}

#[tokio::test]
async fn the_feed_shows_past_and_live_activity() {
    let (h, tui) = after_a_review().await;

    tui.press(KeyCode::Char('2'));

    // Earlier events come from the backlog, later ones arrived live.
    tui.wait_for("connected to GitHub as @octo").await;
    tui.wait_for("queued for review: Rate limit login #7").await;
    tui.wait_for("reviewing with Claude").await;
    tui.wait_for("the agent's review is enough").await;
    tui.wait_for("posted https://github.com/acme/api/pull/7#pullrequestreview-907")
        .await;
    assert!(tui.screen().contains("acme/api#7"));
    assert!(
        !tui.screen().contains("checked GitHub"),
        "polls hidden by default"
    );
    // And something new shows up while watching.
    h.client()
        .save_policy(
            &prowld::policy::Scope::Global,
            "## ALWAYS require human review\n- [X] x\n",
        )
        .await
        .unwrap();
    tui.wait_for("policy global updated to v").await;
    tui.press(KeyCode::Char('p'));
    tui.wait_for("checked GitHub").await;

    tui.press(KeyCode::Char('f'));
    tui.type_text("watching");
    tui.press(KeyCode::Enter);
    tui.wait_for("filter: watching").await;
    assert!(!tui.screen().contains("reviewing with Claude"));
    tui.press(KeyCode::Char('c'));
    tui.press(KeyCode::Char(' '));
    tui.wait_for("paused").await;
    tui.quit().await;
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn history_lists_reviews_with_details_and_opens_them() {
    let (h, tui) = after_a_review().await;

    tui.press(KeyCode::Char('3'));

    tui.wait_for("acme/api#7").await;
    tui.wait_for("agent").await;
    assert!(tui.screen().contains("Rate limit login #7"));
    assert!(tui.screen().contains("$0.05"));
    tui.press(KeyCode::Enter);
    tui.wait_for("Summary: Renames a helper.").await;
    tui.wait_for("The agent's review was enough.").await;
    tui.press(KeyCode::Char('o'));
    let url = "https://github.com/acme/api/pull/7#pullrequestreview-907";
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !OPENED.lock().unwrap().iter().any(|u| u == url) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the review was never opened"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tui.press(KeyCode::Esc);
    tui.wait_for("severity").await;
    tui.quit().await;
    h.shutdown().await.unwrap();
}
