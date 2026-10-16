//! TUI state and the pure `update` that turns messages into state changes and commands.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use prowld::{
    AuthState, Costs, Doctor, DryRun, EffectivePolicy, Event, PolicySummary, PolicyVersion,
    PowerStatus, Repo, RepoPage, Status,
};

use crate::policy_tab::{DryRunState, PolicyTab};

pub const PER_PAGE: usize = 15;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Connecting,
    DaemonDown,
    Doctor,
    Login,
    Repos,
    /// First-run step after picking repos: look at (and optionally edit) the policy.
    PolicySetup,
    Main,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Queue,
    Feed,
    History,
    Policy,
    Repos,
    Settings,
}

impl Tab {
    pub const ALL: [Tab; 6] = [
        Tab::Queue,
        Tab::Feed,
        Tab::History,
        Tab::Policy,
        Tab::Repos,
        Tab::Settings,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Queue => "Queue",
            Tab::Feed => "Feed",
            Tab::History => "History",
            Tab::Policy => "Policy",
            Tab::Repos => "Repos",
            Tab::Settings => "Settings",
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }
}

#[derive(Debug, Clone, Default)]
pub struct RepoPicker {
    pub query: String,
    pub searching: bool,
    pub page: Option<RepoPage>,
    pub page_no: usize,
    pub selected: usize,
    pub loading: bool,
    /// Id of the latest request; answers to older ones (e.g. while typing) are dropped.
    pub request: u64,
}

#[derive(Debug, Clone, Default)]
pub struct StatusBar {
    pub account: Option<String>,
    pub watched: Option<usize>,
    pub queue: Option<usize>,
    pub power: Option<PowerStatus>,
    pub costs: Option<Costs>,
}

/// Results of work the runtime did for us.
#[derive(Debug, Clone)]
pub enum Msg {
    Key(KeyEvent),
    Tick,
    Connected(Result<Status, String>),
    Doctor(Result<Doctor, String>),
    Auth(Result<AuthState, String>),
    WatchedCount(Result<usize, String>),
    /// The answer to the `LoadRepos` request with this id.
    Repos(u64, Result<RepoPage, String>),
    WatchToggled(Result<Repo, String>),
    StatusBar(StatusBar),
    Event(Event),
    ServiceInstalled(Result<String, String>),
    Policies(Result<Vec<PolicySummary>, String>),
    Policy(Result<EffectivePolicy, String>),
    Versions(Result<Vec<PolicyVersion>, String>),
    /// The editor closed (or failed to start).
    Edited(Result<(), String>),
    /// An override was created, restored or deleted.
    PolicyChanged(Result<String, String>),
    DryRunDone(Result<DryRun, String>),
}

/// Work for the runtime to do; it answers with a [`Msg`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    Connect,
    Subscribe,
    LoadDoctor,
    LoadAuth,
    LoadWatchedCount,
    StartLogin,
    LoadRepos {
        request: u64,
        query: String,
        page: usize,
        refresh: bool,
    },
    SetWatched {
        full_name: String,
        watch: bool,
    },
    LoadStatusBar,
    InstallService,
    LoadPolicies,
    LoadPolicy {
        scope: String,
    },
    LoadVersions {
        scope: String,
    },
    /// Suspend the UI and open the policy file in the user's editor.
    EditPolicy {
        scope: String,
    },
    /// Create an override (seeded from the global policy) and open it in the editor.
    CreateOverride {
        repo: String,
    },
    DeleteOverride {
        scope: String,
    },
    RestorePolicy {
        scope: String,
        version_id: i64,
    },
    DryRun {
        repo: String,
        pr: u64,
    },
}

#[derive(Debug, Clone)]
pub struct App {
    pub screen: Screen,
    pub tab: Tab,
    pub endpoint: String,
    pub daemon_error: Option<String>,
    pub doctor: Option<Doctor>,
    pub auth: Option<AuthState>,
    pub repos: RepoPicker,
    pub policy: PolicyTab,
    pub status: StatusBar,
    /// A one-line message shown above the status bar.
    pub notice: Option<String>,
    pub help: bool,
    pub quit: bool,
    /// First-run setup, as opposed to managing repos from the main screen.
    pub onboarding: bool,
}

impl App {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            screen: Screen::Connecting,
            tab: Tab::Queue,
            endpoint: endpoint.into(),
            daemon_error: None,
            doctor: None,
            auth: None,
            repos: RepoPicker::default(),
            policy: PolicyTab::default(),
            status: StatusBar::default(),
            notice: None,
            help: false,
            quit: false,
            onboarding: false,
        }
    }

    pub fn init(&mut self) -> Vec<Cmd> {
        vec![Cmd::Connect]
    }

    pub fn update(&mut self, msg: Msg) -> Vec<Cmd> {
        match msg {
            Msg::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key),
            Msg::Key(_) => vec![],
            Msg::Tick => match self.screen {
                Screen::Connecting | Screen::DaemonDown => vec![],
                _ => vec![Cmd::LoadStatusBar],
            },
            Msg::Connected(Ok(_)) => {
                self.daemon_error = None;
                vec![Cmd::Subscribe, Cmd::LoadDoctor, Cmd::LoadStatusBar]
            }
            Msg::Connected(Err(e)) => {
                self.screen = Screen::DaemonDown;
                self.daemon_error = Some(e);
                vec![]
            }
            Msg::Doctor(Ok(doctor)) => {
                let ok = doctor.ok();
                self.doctor = Some(doctor);
                if ok {
                    vec![Cmd::LoadAuth]
                } else {
                    self.screen = Screen::Doctor;
                    vec![]
                }
            }
            Msg::Doctor(Err(e)) => self.lost(e),
            Msg::Auth(Ok(auth)) => {
                let connected = matches!(auth, AuthState::Connected { .. });
                self.auth = Some(auth);
                if connected {
                    vec![Cmd::LoadWatchedCount]
                } else {
                    self.screen = Screen::Login;
                    vec![]
                }
            }
            Msg::Auth(Err(e)) => self.lost(e),
            Msg::WatchedCount(Ok(n)) => {
                self.status.watched = Some(n);
                if matches!(self.screen, Screen::Main | Screen::Repos) {
                    return vec![];
                }
                if n == 0 {
                    self.onboarding = true;
                    self.screen = Screen::Repos;
                    self.load_repos(false)
                } else {
                    self.screen = Screen::Main;
                    vec![Cmd::LoadStatusBar]
                }
            }
            Msg::WatchedCount(Err(e)) => self.lost(e),
            Msg::Repos(request, _) if request != self.repos.request => vec![],
            Msg::Repos(_, Ok(page)) => {
                self.repos.loading = false;
                self.repos.selected = self.repos.selected.min(page.repos.len().saturating_sub(1));
                self.repos.page = Some(page);
                vec![]
            }
            Msg::Repos(_, Err(e)) => {
                self.repos.loading = false;
                self.notice = Some(e);
                vec![]
            }
            Msg::WatchToggled(Ok(repo)) => {
                // Count it now: finishing onboarding checks the count, and the status bar
                // refresh that would update it arrives later.
                let delta: isize = if repo.watched { 1 } else { -1 };
                let was = self
                    .repos
                    .page
                    .as_ref()
                    .and_then(|p| p.repos.iter().find(|r| r.id == repo.id))
                    .map(|r| r.watched);
                if was != Some(repo.watched) {
                    let count = self.status.watched.unwrap_or(0) as isize + delta;
                    self.status.watched = Some(count.max(0) as usize);
                }
                if let Some(page) = &mut self.repos.page
                    && let Some(r) = page.repos.iter_mut().find(|r| r.id == repo.id)
                {
                    *r = repo;
                }
                vec![Cmd::LoadStatusBar]
            }
            Msg::WatchToggled(Err(e)) => {
                self.notice = Some(e);
                vec![]
            }
            Msg::StatusBar(bar) => {
                self.status = bar;
                vec![]
            }
            Msg::Event(event) => self.on_event(&event),
            Msg::ServiceInstalled(Ok(message)) => {
                self.notice = Some(message);
                self.screen = Screen::Connecting;
                vec![Cmd::Connect]
            }
            Msg::ServiceInstalled(Err(e)) => {
                self.notice = Some(format!("Could not install the service: {e}"));
                vec![]
            }
            Msg::Policies(Ok(list)) => self.policy.on_policies(list),
            Msg::Policy(Ok(policy)) => {
                if self.policy.selected_scope().as_deref() == Some(policy.scope.as_str()) {
                    self.policy.content = Some(policy);
                }
                vec![]
            }
            Msg::Versions(Ok(versions)) => {
                if self.policy.versions.is_some() {
                    self.policy.version_selected = self
                        .policy
                        .version_selected
                        .min(versions.len().saturating_sub(1));
                    self.policy.versions = Some(versions);
                }
                vec![]
            }
            Msg::Edited(Ok(())) => vec![Cmd::LoadPolicies],
            Msg::PolicyChanged(Ok(message)) => {
                self.notice = Some(message);
                vec![Cmd::LoadPolicies]
            }
            Msg::DryRunDone(result) => {
                if let Some(DryRunState::Running { target }) = self.policy.dry_run.take() {
                    self.policy.dry_run = Some(DryRunState::Done {
                        target,
                        result: Box::new(result),
                    });
                }
                vec![Cmd::LoadStatusBar]
            }
            Msg::Policies(Err(e))
            | Msg::Policy(Err(e))
            | Msg::Versions(Err(e))
            | Msg::Edited(Err(e))
            | Msg::PolicyChanged(Err(e)) => {
                self.notice = Some(e);
                vec![]
            }
        }
    }

    fn lost(&mut self, error: String) -> Vec<Cmd> {
        self.screen = Screen::DaemonDown;
        self.daemon_error = Some(error);
        vec![]
    }

    fn on_event(&mut self, event: &Event) -> Vec<Cmd> {
        match event.kind.as_str() {
            "auth.connected" | "auth.failed" | "auth.disconnected" | "auth.reconnect_required" => {
                if event.kind != "auth.connected" && self.screen == Screen::Main {
                    self.screen = Screen::Login;
                }
                vec![Cmd::LoadAuth, Cmd::LoadStatusBar]
            }
            "auth.pending" => vec![Cmd::LoadAuth],
            "policy.changed" if !self.policy.list.is_empty() => vec![Cmd::LoadPolicies],
            kind if kind.starts_with("repo.")
                || kind.starts_with("queue.")
                || kind.starts_with("review.")
                || kind.starts_with("power.")
                || kind.starts_with("budget.") =>
            {
                vec![Cmd::LoadStatusBar]
            }
            _ => vec![],
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> Vec<Cmd> {
        let ctrl_c =
            key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl_c {
            self.quit = true;
            return vec![];
        }
        if self.repos.searching && self.picker_visible() {
            return self.on_search_key(key);
        }
        if self.screen == Screen::Main && self.tab == Tab::Policy && self.policy.captures_keys() {
            return self.policy.on_key(key);
        }
        if self.help {
            self.help = false;
            return vec![];
        }
        match key.code {
            KeyCode::Char('q') => {
                self.quit = true;
                return vec![];
            }
            KeyCode::Char('?') => {
                self.help = true;
                return vec![];
            }
            _ => {}
        }
        self.notice = None;
        match self.screen {
            Screen::Connecting => vec![],
            Screen::DaemonDown => match key.code {
                KeyCode::Char('r') => {
                    self.screen = Screen::Connecting;
                    vec![Cmd::Connect]
                }
                KeyCode::Char('i') => {
                    self.notice = Some("Installing the background service…".into());
                    vec![Cmd::InstallService]
                }
                _ => vec![],
            },
            Screen::Doctor => match key.code {
                KeyCode::Char('r') => vec![Cmd::LoadDoctor],
                KeyCode::Enter => vec![Cmd::LoadAuth],
                _ => vec![],
            },
            Screen::Login => match key.code {
                KeyCode::Enter => vec![Cmd::StartLogin],
                _ => vec![],
            },
            Screen::Repos => {
                if key.code == KeyCode::Enter {
                    if self.status.watched.unwrap_or(0) == 0 {
                        self.notice =
                            Some("Watch at least one repository (space) to continue.".into());
                        return vec![];
                    }
                    self.screen = Screen::PolicySetup;
                    return vec![];
                }
                self.on_picker_key(key)
            }
            Screen::PolicySetup => match key.code {
                KeyCode::Char('e') => vec![Cmd::EditPolicy {
                    scope: "global".into(),
                }],
                KeyCode::Enter => {
                    self.onboarding = false;
                    self.screen = Screen::Main;
                    self.tab = Tab::Queue;
                    vec![Cmd::LoadStatusBar]
                }
                _ => vec![],
            },
            Screen::Main => self.on_main_key(key),
        }
    }

    fn on_main_key(&mut self, key: KeyEvent) -> Vec<Cmd> {
        let next = match key.code {
            KeyCode::Tab => Some((self.tab.index() + 1) % Tab::ALL.len()),
            KeyCode::BackTab => Some((self.tab.index() + Tab::ALL.len() - 1) % Tab::ALL.len()),
            KeyCode::Char(c @ '1'..='6') => Some(c as usize - '1' as usize),
            _ => None,
        };
        if let Some(i) = next {
            self.tab = Tab::ALL[i];
            if self.tab == Tab::Repos && self.repos.page.is_none() {
                return self.load_repos(false);
            }
            if self.tab == Tab::Policy {
                return vec![Cmd::LoadPolicies];
            }
            return vec![];
        }
        match self.tab {
            Tab::Repos => return self.on_picker_key(key),
            Tab::Policy => return self.policy.on_key(key),
            _ => {}
        }
        vec![]
    }

    fn picker_visible(&self) -> bool {
        self.screen == Screen::Repos || (self.screen == Screen::Main && self.tab == Tab::Repos)
    }

    fn on_picker_key(&mut self, key: KeyEvent) -> Vec<Cmd> {
        let len = self.repos.page.as_ref().map_or(0, |p| p.repos.len());
        match key.code {
            KeyCode::Char('/') => {
                self.repos.searching = true;
                vec![]
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.repos.selected + 1 < len {
                    self.repos.selected += 1;
                }
                vec![]
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.repos.selected = self.repos.selected.saturating_sub(1);
                vec![]
            }
            KeyCode::Char('n') | KeyCode::PageDown => {
                let pages = self
                    .repos
                    .page
                    .as_ref()
                    .map_or(1, |p| p.total.div_ceil(PER_PAGE).max(1));
                if self.repos.page_no + 1 < pages {
                    self.repos.page_no += 1;
                    self.repos.selected = 0;
                    return self.load_repos(false);
                }
                vec![]
            }
            KeyCode::Char('p') | KeyCode::PageUp => {
                if self.repos.page_no > 0 {
                    self.repos.page_no -= 1;
                    self.repos.selected = 0;
                    return self.load_repos(false);
                }
                vec![]
            }
            KeyCode::Char('r') => self.load_repos(true),
            KeyCode::Char(' ') => {
                let Some(repo) = self
                    .repos
                    .page
                    .as_ref()
                    .and_then(|p| p.repos.get(self.repos.selected))
                else {
                    return vec![];
                };
                if repo.archived && !repo.watched {
                    self.notice = Some(format!(
                        "{} is archived and can't be watched.",
                        repo.full_name
                    ));
                    return vec![];
                }
                vec![Cmd::SetWatched {
                    full_name: repo.full_name.clone(),
                    watch: !repo.watched,
                }]
            }
            _ => vec![],
        }
    }

    fn on_search_key(&mut self, key: KeyEvent) -> Vec<Cmd> {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => {
                self.repos.searching = false;
                vec![]
            }
            KeyCode::Backspace => {
                self.repos.query.pop();
                self.repos.page_no = 0;
                self.repos.selected = 0;
                self.load_repos(false)
            }
            KeyCode::Char(c) => {
                self.repos.query.push(c);
                self.repos.page_no = 0;
                self.repos.selected = 0;
                self.load_repos(false)
            }
            _ => vec![],
        }
    }

    fn load_repos(&mut self, refresh: bool) -> Vec<Cmd> {
        self.repos.loading = true;
        self.repos.request += 1;
        vec![Cmd::LoadRepos {
            request: self.repos.request,
            query: self.repos.query.clone(),
            page: self.repos.page_no + 1,
            refresh,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> Msg {
        Msg::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn repo(id: i64, name: &str, watched: bool, archived: bool) -> Repo {
        Repo {
            id,
            full_name: name.into(),
            owner: name.split('/').next().unwrap().into(),
            name: name.split('/').nth(1).unwrap().into(),
            private: false,
            archived,
            default_branch: "main".into(),
            html_url: String::new(),
            watched,
        }
    }

    fn page(repos: Vec<Repo>) -> RepoPage {
        RepoPage {
            total: repos.len(),
            page: 1,
            per_page: PER_PAGE,
            repos,
        }
    }

    #[test]
    fn startup_connects_then_checks_tools_then_auth() {
        let mut app = App::new("sock");
        assert_eq!(app.init(), [Cmd::Connect]);
        let status = Status {
            version: "0.1.0".into(),
            pid: 1,
            started_at_ms: 0,
            uptime_secs: 0,
            github: AuthState::Disconnected,
        };
        assert_eq!(
            app.update(Msg::Connected(Ok(status))),
            [Cmd::Subscribe, Cmd::LoadDoctor, Cmd::LoadStatusBar]
        );
        assert_eq!(app.update(Msg::Doctor(Ok(healthy()))), [Cmd::LoadAuth]);
        app.update(Msg::Auth(Ok(AuthState::Disconnected)));
        assert_eq!(app.screen, Screen::Login);
        assert_eq!(app.update(key(KeyCode::Enter)), [Cmd::StartLogin]);
    }

    #[test]
    fn a_connected_account_without_watched_repos_goes_to_the_picker() {
        let mut app = App::new("sock");
        app.update(Msg::Auth(Ok(AuthState::Connected {
            login: "octo".into(),
            name: None,
        })));
        let cmds = app.update(Msg::WatchedCount(Ok(0)));
        assert_eq!(app.screen, Screen::Repos);
        assert!(app.onboarding);
        assert!(matches!(&cmds[..], [Cmd::LoadRepos { page: 1, .. }]));
        // Can't finish onboarding without watching something.
        app.update(key(KeyCode::Enter));
        assert_eq!(app.screen, Screen::Repos);
        assert!(app.notice.is_some());
    }

    #[test]
    fn the_picker_searches_moves_and_toggles() {
        let mut app = App::new("sock");
        app.screen = Screen::Repos;
        app.update(Msg::Repos(
            0,
            Ok(page(vec![
                repo(1, "acme/api", false, false),
                repo(2, "acme/old", false, true),
            ])),
        ));

        app.update(key(KeyCode::Char('/')));
        let cmds = app.update(key(KeyCode::Char('q')));
        assert!(!app.quit, "q types while searching");
        assert_eq!(
            cmds,
            [Cmd::LoadRepos {
                request: 1,
                query: "q".into(),
                page: 1,
                refresh: false
            }]
        );
        app.update(key(KeyCode::Esc));

        assert_eq!(
            app.update(key(KeyCode::Char(' '))),
            [Cmd::SetWatched {
                full_name: "acme/api".into(),
                watch: true
            }]
        );
        app.update(key(KeyCode::Down));
        assert!(app.update(key(KeyCode::Char(' '))).is_empty(), "archived");
        assert!(app.notice.as_deref().unwrap().contains("archived"));
    }

    #[test]
    fn answers_to_superseded_searches_are_ignored() {
        let mut app = App::new("sock");
        app.screen = Screen::Repos;
        app.update(key(KeyCode::Char('/')));
        app.update(key(KeyCode::Char('p')));
        app.update(key(KeyCode::Char('a')));
        // "pa" answers first; then the late answer to the superseded "p" arrives.
        app.update(Msg::Repos(
            2,
            Ok(page(vec![repo(2, "acme/payments", false, false)])),
        ));
        app.update(Msg::Repos(
            1,
            Ok(page(vec![
                repo(1, "acme/api", false, false),
                repo(2, "acme/payments", false, false),
            ])),
        ));
        assert_eq!(app.repos.page.unwrap().total, 1);
    }

    #[test]
    fn watching_counts_at_once_so_onboarding_can_finish() {
        let mut app = App::new("sock");
        app.screen = Screen::Repos;
        app.onboarding = true;
        app.status.watched = Some(0);
        app.update(Msg::Repos(
            0,
            Ok(page(vec![repo(1, "acme/api", false, false)])),
        ));
        app.update(Msg::WatchToggled(Ok(repo(1, "acme/api", true, false))));
        assert_eq!(app.status.watched, Some(1));
        app.update(key(KeyCode::Enter));
        assert_ne!(
            app.screen,
            Screen::Repos,
            "no stale 'watch at least one' refusal"
        );
    }

    #[test]
    fn onboarding_ends_with_the_policy_step() {
        let mut app = App::new("sock");
        app.screen = Screen::Repos;
        app.onboarding = true;
        app.status.watched = Some(1);
        app.update(key(KeyCode::Enter));
        assert_eq!(app.screen, Screen::PolicySetup);
        assert_eq!(
            app.update(key(KeyCode::Char('e'))),
            [Cmd::EditPolicy {
                scope: "global".into()
            }]
        );
        app.update(key(KeyCode::Enter));
        assert_eq!(app.screen, Screen::Main);
    }

    #[test]
    fn typing_in_a_policy_prompt_doesnt_trigger_shortcuts() {
        let mut app = App::new("sock");
        app.screen = Screen::Main;
        app.tab = Tab::Policy;
        app.update(key(KeyCode::Char('t')));
        app.update(key(KeyCode::Char('q')));
        app.update(key(KeyCode::Char('1')));
        assert!(!app.quit);
        assert_eq!(app.tab, Tab::Policy);
        assert_eq!(app.policy.input.as_ref().unwrap().text, "q1");
    }

    #[test]
    fn tabs_cycle_and_jump() {
        let mut app = App::new("sock");
        app.screen = Screen::Main;
        app.update(key(KeyCode::Tab));
        assert_eq!(app.tab, Tab::Feed);
        app.update(key(KeyCode::BackTab));
        app.update(key(KeyCode::BackTab));
        assert_eq!(app.tab, Tab::Settings);
        let cmds = app.update(key(KeyCode::Char('5')));
        assert_eq!(app.tab, Tab::Repos);
        assert!(
            matches!(&cmds[..], [Cmd::LoadRepos { .. }]),
            "repos load on first visit"
        );
    }

    fn healthy() -> Doctor {
        let mut d = Doctor::default();
        d.claude.tool.found = true;
        d.git.found = true;
        d
    }
}
