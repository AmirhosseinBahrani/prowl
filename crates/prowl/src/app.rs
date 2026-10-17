//! TUI state and the pure `update` that turns messages into state changes and commands.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use prowld::{
    AuthState, Costs, Doctor, DryRun, EffectivePolicy, Event, PolicySummary, PolicyVersion,
    PowerStatus, Repo, RepoPage, Status,
};

use crate::feed_tab::FeedTab;
use crate::history_tab::HistoryTab;
use crate::policy_tab::{DryRunState, PolicyTab};
use crate::queue_tab::{QueueAction, QueueTab};
use crate::reviews_tab::ReviewsTab;
use crate::settings_tab::SettingsTab;

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
    Reviews,
    History,
    Policy,
    Repos,
    Settings,
}

impl Tab {
    pub const ALL: [Tab; 7] = [
        Tab::Queue,
        Tab::Feed,
        Tab::Reviews,
        Tab::History,
        Tab::Policy,
        Tab::Repos,
        Tab::Settings,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Queue => "Queue",
            Tab::Feed => "Feed",
            Tab::Reviews => "Reviews",
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
    /// The latest PRowl release, as the daemon last checked it.
    pub update: Option<prowld::updates::UpdateInfo>,
}

/// Results of work the runtime did for us.
#[derive(Debug, Clone)]
pub enum Msg {
    Key(KeyEvent),
    Tick,
    Connected(Result<Status, String>),
    Doctor(Result<Doctor, String>),
    Auth(Result<AuthState, String>),
    /// The answer to starting a device flow or signing in with a token.
    SignIn(Result<AuthState, String>),
    WatchedCount(Result<usize, String>),
    /// The answer to the `LoadRepos` request with this id.
    Repos(u64, Result<RepoPage, String>),
    WatchToggled(Result<Repo, String>),
    StatusBar(StatusBar),
    Event(Event),
    ServiceInstalled(Result<String, String>),
    Policies(Result<Vec<PolicySummary>, String>),
    Policy(Result<EffectivePolicy, String>),
    /// The override for this repository no longer exists (e.g. it was just deleted).
    PolicyGone(String),
    Versions(Result<Vec<PolicyVersion>, String>),
    /// The editor closed (or failed to start).
    Edited(Result<(), String>),
    /// An override was created, restored or deleted.
    PolicyChanged(Result<String, String>),
    DryRunDone(Result<DryRun, String>),
    RecentEvents(Result<Vec<Event>, String>),
    Reviews(Result<Vec<prowld::Review>, String>),
    Jobs(Result<Vec<prowld::Job>, String>),
    Rerun(Result<prowld::Job, String>),
    ServiceStopped(Result<(), String>),
    JobCancelled(Result<prowld::Cancelled, String>),
    Verify(Result<prowld::Job, String>),
    /// The transcript of this job.
    Transcript(
        i64,
        Result<Vec<prowld::transcripts::TranscriptEntry>, String>,
    ),
    Opened(Result<(), String>),
    QueueItems(Result<Vec<prowld::QueueItem>, String>),
    QueueDetail(Result<prowld::QueueDetail, String>),
    Threads {
        repo: String,
        pr: u64,
        result: Result<Vec<prowld::ReviewThread>, String>,
    },
    Resolved {
        repo: String,
        number: u64,
        action: QueueAction,
        result: Result<prowld::QueueItem, String>,
    },
    Settings(Result<prowld::Settings, String>),
    SettingsSaved(Result<prowld::Settings, String>),
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
    SignOut,
    /// Stop the background service (it stays installed).
    StopService,
    /// Start the installed background service again.
    StartService,
    /// Stop a waiting or running review.
    CancelJob {
        job_id: i64,
    },
    SignInWithToken {
        token: String,
    },
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
    LoadRecentEvents,
    LoadReviews,
    LoadJobs,
    /// Review this PR again at its current commit.
    Rerun {
        repo: String,
        pr: u64,
    },
    /// Check this PR's findings were addressed and approve it if so.
    Verify {
        repo: String,
        pr: u64,
    },
    LoadTranscript {
        job_id: i64,
    },
    OpenUrl {
        url: String,
    },
    LoadQueue,
    /// The PR's review threads.
    LoadThreads {
        repo: String,
        pr: u64,
    },
    LoadQueueItem {
        repo: String,
        number: u64,
    },
    Resolve {
        repo: String,
        number: u64,
        action: QueueAction,
        comment: String,
    },
    LoadSettings,
    SaveSettings {
        patch: serde_json::Value,
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
    /// The personal access token being typed on the login screen.
    pub token_input: Option<String>,
    /// Why the last sign-in attempt was refused.
    pub login_error: Option<String>,
    /// "Sign out of GitHub?" is waiting for an answer (Settings tab).
    pub confirm_sign_out: bool,
    /// "Stop the background service and quit?" is waiting for an answer.
    pub confirm_stop_service: bool,
    pub repos: RepoPicker,
    pub policy: PolicyTab,
    pub feed: FeedTab,
    pub queue: QueueTab,
    pub settings: SettingsTab,
    pub history: HistoryTab,
    pub reviews: ReviewsTab,
    pub status: StatusBar,
    /// A one-line message shown above the status bar.
    pub notice: Option<String>,
    pub help: bool,
    pub quit: bool,
    /// First-run setup, as opposed to managing repos from the main screen.
    pub onboarding: bool,
    /// Enter was pressed in the onboarding picker; finish once the daemon confirms the count.
    pub confirming_repos: bool,
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
            token_input: None,
            login_error: None,
            confirm_sign_out: false,
            confirm_stop_service: false,
            repos: RepoPicker::default(),
            policy: PolicyTab::default(),
            feed: FeedTab::default(),
            queue: QueueTab::default(),
            settings: SettingsTab::default(),
            history: HistoryTab::default(),
            reviews: ReviewsTab::default(),
            status: StatusBar::default(),
            notice: None,
            help: false,
            quit: false,
            onboarding: false,
            confirming_repos: false,
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
            Msg::SignIn(Ok(auth)) => {
                self.token_input = None;
                self.login_error = None;
                self.update(Msg::Auth(Ok(auth)))
            }
            Msg::SignIn(Err(e)) => {
                self.login_error = Some(e);
                vec![]
            }
            Msg::WatchedCount(Ok(n)) => {
                self.status.watched = Some(n);
                if self.screen == Screen::Repos && std::mem::take(&mut self.confirming_repos) {
                    if n == 0 {
                        self.notice =
                            Some("Watch at least one repository (space) to continue.".into());
                    } else {
                        self.screen = Screen::PolicySetup;
                    }
                    return vec![];
                }
                if matches!(self.screen, Screen::Main | Screen::Repos) {
                    return vec![];
                }
                if n == 0 {
                    self.onboarding = true;
                    self.screen = Screen::Repos;
                    self.load_repos(false)
                } else {
                    self.screen = Screen::Main;
                    vec![Cmd::LoadStatusBar, Cmd::LoadQueue]
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
            Msg::ServiceStopped(Ok(())) => {
                self.quit = true;
                vec![]
            }
            Msg::ServiceStopped(Err(e)) => {
                self.notice = Some(format!("Could not stop the background service: {e}"));
                vec![]
            }
            Msg::JobCancelled(Ok(cancelled)) => {
                self.notice = Some(match cancelled {
                    prowld::Cancelled::Waiting => "Cancelled before it started.".into(),
                    prowld::Cancelled::Running => {
                        "Stopped: Claude was stopped and nothing more is posted.".into()
                    }
                });
                self.reviews.refresh()
            }
            Msg::JobCancelled(Err(e)) => {
                self.notice = Some(format!("Couldn't cancel: {e}"));
                vec![]
            }
            Msg::ServiceInstalled(Err(e)) => {
                self.notice = Some(format!("Could not install the service: {e}"));
                vec![]
            }
            Msg::Policies(Ok(list)) => self.policy.on_policies(list),
            // A list fetched before a delete can still name the override; fetch it again.
            Msg::PolicyGone(scope) => {
                if self.policy.content.as_ref().map(|p| p.scope.as_str()) == Some(scope.as_str()) {
                    self.policy.content = None;
                }
                vec![Cmd::LoadPolicies]
            }
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
            Msg::RecentEvents(Ok(events)) => {
                self.feed.on_recent(events);
                vec![]
            }
            Msg::Reviews(Ok(reviews)) => {
                self.reviews.reviews = reviews.clone();
                self.history.on_reviews(reviews);
                vec![]
            }
            Msg::Jobs(Ok(jobs)) => self.reviews.on_jobs(jobs),
            Msg::Rerun(Ok(job)) => {
                self.notice = Some(format!(
                    "Re-reviewing {}#{} — follow it in the Reviews tab (3).",
                    job.repo, job.pr_number
                ));
                self.reviews.refresh()
            }
            Msg::Rerun(Err(e)) => {
                self.notice = Some(format!("Couldn't re-review: {e}"));
                vec![]
            }
            Msg::Verify(Ok(job)) => {
                self.notice = Some(format!(
                    "Verifying {}#{}: PRowl approves it if every finding was addressed — follow \
                     it in the Reviews tab (3).",
                    job.repo, job.pr_number
                ));
                self.reviews.refresh()
            }
            Msg::Verify(Err(e)) => {
                self.notice = Some(format!("Couldn't verify: {e}"));
                vec![]
            }
            Msg::Jobs(Err(e)) => {
                self.reviews.on_jobs_failed();
                self.notice = Some(e);
                vec![]
            }
            Msg::Transcript(job_id, Ok(entries)) => self.reviews.on_transcript(job_id, entries),
            Msg::Transcript(job_id, Err(e)) => {
                self.reviews.on_transcript_failed(job_id);
                self.notice = Some(e);
                vec![]
            }
            Msg::Opened(Ok(())) => vec![],
            Msg::QueueItems(Ok(items)) => {
                self.queue.on_items(items);
                vec![]
            }
            Msg::QueueDetail(Ok(detail)) => self.queue.on_detail(detail),
            Msg::Threads { repo, pr, result } => {
                self.queue.on_threads(&repo, pr, result);
                vec![]
            }
            Msg::Resolved {
                repo,
                number,
                action,
                result: Ok(_),
            } => {
                self.queue.on_resolved(&repo, number);
                self.notice = Some(format!("{} {repo}#{number}.", action.past_tense()));
                vec![Cmd::LoadQueue, Cmd::LoadStatusBar]
            }
            Msg::Resolved { result: Err(e), .. } => {
                self.queue.busy = false;
                self.queue.error = Some(e);
                vec![]
            }
            Msg::Settings(Ok(settings)) => {
                self.settings.settings = Some(settings);
                vec![]
            }
            Msg::SettingsSaved(result) => {
                self.settings.on_saved(result);
                vec![Cmd::LoadStatusBar]
            }
            Msg::Settings(Err(e)) => {
                self.notice = Some(e);
                vec![]
            }
            Msg::QueueItems(Err(e)) | Msg::QueueDetail(Err(e)) => {
                self.notice = Some(e);
                vec![]
            }
            Msg::RecentEvents(Err(e)) | Msg::Reviews(Err(e)) | Msg::Opened(Err(e)) => {
                self.notice = Some(e);
                vec![]
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
        self.feed.push(event.clone());
        let mut cmds = self.react_to(event);
        if self.history.loaded
            && matches!(event.kind.as_str(), "review.decided" | "review.published")
        {
            cmds.push(Cmd::LoadReviews);
        }
        if event.kind.starts_with("queue.") {
            cmds.push(Cmd::LoadQueue);
        }
        let job_id = event.payload.get("job_id").and_then(|v| v.as_i64());
        for cmd in self.reviews.on_event(&event.kind, job_id) {
            if !cmds.contains(&cmd) {
                cmds.push(cmd);
            }
        }
        cmds
    }

    fn react_to(&mut self, event: &Event) -> Vec<Cmd> {
        match event.kind.as_str() {
            "auth.connected" | "auth.failed" | "auth.disconnected" | "auth.reconnect_required" => {
                if event.kind != "auth.connected" && self.screen == Screen::Main {
                    self.screen = Screen::Login;
                }
                vec![Cmd::LoadAuth, Cmd::LoadStatusBar]
            }
            "auth.pending" => vec![Cmd::LoadAuth],
            "policy.changed" if !self.policy.list.is_empty() => vec![Cmd::LoadPolicies],
            "settings.changed" if self.settings.settings.is_some() => vec![Cmd::LoadSettings],
            kind if kind.starts_with("repo.")
                || kind.starts_with("queue.")
                || kind.starts_with("review.")
                || kind.starts_with("power.")
                || kind.starts_with("budget.")
                || kind.starts_with("update.") =>
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
        if self.screen == Screen::Login && self.token_input.is_some() {
            return self.on_token_key(key);
        }
        if std::mem::take(&mut self.confirm_stop_service) {
            self.notice = None;
            return if key.code == KeyCode::Char('y') {
                self.notice = Some("Stopping PRowl's background service…".into());
                vec![Cmd::StopService]
            } else {
                vec![]
            };
        }
        if std::mem::take(&mut self.confirm_sign_out) {
            return if key.code == KeyCode::Char('y') {
                self.notice = Some("Signing out of GitHub…".into());
                vec![Cmd::SignOut]
            } else {
                vec![]
            };
        }
        if self.screen == Screen::Main {
            match self.tab {
                Tab::Policy if self.policy.captures_keys() => return self.policy.on_key(key),
                Tab::Feed if self.feed.captures_keys() => return self.feed.on_key(key),
                Tab::History if self.history.captures_keys() => return self.history.on_key(key),
                Tab::Queue if self.queue.captures_keys() => return self.queue.on_key(key),
                Tab::Settings if self.settings.captures_keys() => return self.settings.on_key(key),
                Tab::Reviews if self.reviews.captures_keys() => return self.reviews.on_key(key),
                _ => {}
            }
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
            // Quit and stop the background service too (only while it's running).
            KeyCode::Char('Q') if self.screen != Screen::DaemonDown => {
                self.confirm_stop_service = true;
                self.notice = Some(
                    "Stop PRowl's background service and quit? Nothing is reviewed until it \
                     starts again (prowl → [s], prowld service start, or your next login). \
                     [y] yes · any other key: no"
                        .into(),
                );
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
                KeyCode::Char('s') => {
                    self.notice = Some("Starting the background service…".into());
                    vec![Cmd::StartService]
                }
                _ => vec![],
            },
            Screen::Doctor => match key.code {
                KeyCode::Char('r') => vec![Cmd::LoadDoctor],
                KeyCode::Enter => vec![Cmd::LoadAuth],
                _ => vec![],
            },
            Screen::Login => match key.code {
                KeyCode::Enter => {
                    self.login_error = None;
                    vec![Cmd::StartLogin]
                }
                KeyCode::Char('t') => {
                    self.login_error = None;
                    self.token_input = Some(String::new());
                    vec![]
                }
                KeyCode::Char('o') => match &self.auth {
                    Some(AuthState::Pending {
                        verification_uri, ..
                    }) => vec![Cmd::OpenUrl {
                        url: verification_uri.clone(),
                    }],
                    _ => vec![],
                },
                _ => vec![],
            },
            Screen::Repos => {
                if key.code == KeyCode::Enter {
                    // Ask the daemon: the cached count can be overwritten by a status refresh
                    // that started before the last watch.
                    self.confirming_repos = true;
                    return vec![Cmd::LoadWatchedCount];
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
                    vec![Cmd::LoadStatusBar, Cmd::LoadQueue]
                }
                _ => vec![],
            },
            Screen::Main => self.on_main_key(key),
        }
    }

    fn on_token_key(&mut self, key: KeyEvent) -> Vec<Cmd> {
        let Some(token) = &mut self.token_input else {
            return vec![];
        };
        match key.code {
            KeyCode::Esc => {
                self.token_input = None;
                self.login_error = None;
                // Switching account from Settings: back to where you were.
                if matches!(self.auth, Some(AuthState::Connected { .. })) {
                    self.screen = Screen::Main;
                }
            }
            KeyCode::Backspace => {
                token.pop();
            }
            KeyCode::Enter if !token.trim().is_empty() => {
                self.login_error = None;
                return vec![Cmd::SignInWithToken {
                    token: token.trim().to_owned(),
                }];
            }
            KeyCode::Char(c) if !c.is_whitespace() && !c.is_control() => token.push(c),
            _ => {}
        }
        vec![]
    }

    fn on_main_key(&mut self, key: KeyEvent) -> Vec<Cmd> {
        let next = match key.code {
            KeyCode::Tab | KeyCode::Right => Some((self.tab.index() + 1) % Tab::ALL.len()),
            KeyCode::BackTab | KeyCode::Left => {
                Some((self.tab.index() + Tab::ALL.len() - 1) % Tab::ALL.len())
            }
            KeyCode::Char(c @ '1'..='9') if (c as usize - '1' as usize) < Tab::ALL.len() => {
                Some(c as usize - '1' as usize)
            }
            _ => None,
        };
        if let Some(i) = next {
            self.tab = Tab::ALL[i];
            if self.tab == Tab::Repos && self.repos.page.is_none() {
                return self.load_repos(false);
            }
            return match self.tab {
                Tab::Policy => vec![Cmd::LoadPolicies],
                Tab::Feed if !self.feed.loaded => vec![Cmd::LoadRecentEvents],
                Tab::History => vec![Cmd::LoadReviews],
                Tab::Reviews => self.reviews.refresh(),
                Tab::Queue => vec![Cmd::LoadQueue],
                Tab::Settings => vec![Cmd::LoadSettings],
                _ => vec![],
            };
        }
        match self.tab {
            Tab::Repos => self.on_picker_key(key),
            Tab::Policy => self.policy.on_key(key),
            Tab::Feed => self.feed.on_key(key),
            Tab::History => self.history.on_key(key),
            Tab::Reviews => self.reviews.on_key(key),
            Tab::Queue => self.queue.on_key(key),
            Tab::Settings => match key.code {
                KeyCode::Char('l') => {
                    self.confirm_sign_out = true;
                    vec![]
                }
                KeyCode::Char('t') => {
                    self.screen = Screen::Login;
                    self.login_error = None;
                    self.token_input = Some(String::new());
                    vec![]
                }
                _ => self.settings.on_key(key),
            },
        }
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

    #[test]
    fn a_personal_access_token_can_be_typed_on_the_login_screen() {
        let mut app = App::new("test");
        app.update(Msg::Auth(Ok(AuthState::Disconnected)));
        assert_eq!(app.screen, Screen::Login);

        app.update(key(KeyCode::Char('t')));
        for c in "ghp_q?x".chars() {
            app.update(key(KeyCode::Char(c)));
        }
        app.update(key(KeyCode::Backspace));
        assert!(!app.quit, "q is part of the token, not quit");
        assert!(!app.help);
        assert_eq!(
            app.update(key(KeyCode::Enter)),
            [Cmd::SignInWithToken {
                token: "ghp_q?".into()
            }]
        );

        app.update(Msg::SignIn(Err(
            "422: GitHub did not accept this token".into()
        )));
        assert_eq!(app.screen, Screen::Login, "a refused token keeps you here");
        assert!(
            app.login_error
                .as_deref()
                .unwrap()
                .contains("did not accept")
        );
        assert!(app.token_input.is_some());

        let cmds = app.update(Msg::SignIn(Ok(AuthState::Connected {
            login: "octo".into(),
            name: None,
        })));
        assert_eq!(cmds, [Cmd::LoadWatchedCount]);
        assert!(app.token_input.is_none());
    }

    #[test]
    fn escape_leaves_token_entry() {
        let mut app = App::new("test");
        app.update(Msg::Auth(Ok(AuthState::Disconnected)));
        app.update(key(KeyCode::Char('t')));
        app.update(key(KeyCode::Char('x')));
        app.update(key(KeyCode::Esc));
        assert!(app.token_input.is_none());
        assert_eq!(app.update(key(KeyCode::Enter)), [Cmd::StartLogin]);
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
            update: None,
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
        assert_eq!(app.update(key(KeyCode::Enter)), [Cmd::LoadWatchedCount]);
        app.update(Msg::WatchedCount(Ok(0)));
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
        // A status refresh that started before the watch lands afterwards.
        app.update(Msg::StatusBar(StatusBar {
            watched: Some(0),
            ..StatusBar::default()
        }));
        assert_eq!(app.update(key(KeyCode::Enter)), [Cmd::LoadWatchedCount]);
        app.update(Msg::WatchedCount(Ok(1)));
        assert_eq!(
            app.screen,
            Screen::PolicySetup,
            "the daemon's count decides, not a stale refresh"
        );
    }

    #[test]
    fn onboarding_needs_a_watched_repository() {
        let mut app = App::new("sock");
        app.screen = Screen::Repos;
        app.onboarding = true;
        app.update(key(KeyCode::Enter));
        app.update(Msg::WatchedCount(Ok(0)));
        assert_eq!(app.screen, Screen::Repos);
        assert!(
            app.notice
                .as_deref()
                .unwrap()
                .contains("Watch at least one")
        );
    }

    #[test]
    fn onboarding_ends_with_the_policy_step() {
        let mut app = App::new("sock");
        app.screen = Screen::Repos;
        app.onboarding = true;
        app.update(key(KeyCode::Enter));
        app.update(Msg::WatchedCount(Ok(1)));
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
        let cmds = app.update(key(KeyCode::Char('6')));
        assert_eq!(app.tab, Tab::Repos);
        assert!(
            matches!(&cmds[..], [Cmd::LoadRepos { .. }]),
            "repos load on first visit"
        );
    }

    #[test]
    fn arrow_keys_switch_tabs_and_wrap() {
        let mut app = App::new("sock");
        app.screen = Screen::Main;
        assert_eq!(app.update(key(KeyCode::Right)), [Cmd::LoadRecentEvents]);
        assert_eq!(app.tab, Tab::Feed);
        app.update(key(KeyCode::Left));
        app.update(key(KeyCode::Left));
        assert_eq!(
            app.tab,
            Tab::Settings,
            "left from the first tab wraps to the last"
        );
        app.update(key(KeyCode::Right));
        assert_eq!(
            app.tab,
            Tab::Queue,
            "right from the last tab wraps to the first"
        );
    }

    #[test]
    fn arrow_keys_stay_in_a_text_field() {
        let mut app = App::new("sock");
        app.screen = Screen::Main;
        app.tab = Tab::Policy;
        app.update(key(KeyCode::Char('n')));
        app.update(key(KeyCode::Right));
        app.update(key(KeyCode::Left));
        assert_eq!(app.tab, Tab::Policy);
        assert!(app.policy.input.is_some());
    }

    #[test]
    fn a_policy_that_vanished_reloads_the_list_instead_of_showing_an_error() {
        let mut app = App::new("sock");
        app.screen = Screen::Main;
        app.tab = Tab::Policy;
        app.notice = Some("Deleted the override for acme/api.".into());

        let cmds = app.update(Msg::PolicyGone("acme/api".into()));

        assert_eq!(cmds, [Cmd::LoadPolicies]);
        assert_eq!(
            app.notice.as_deref(),
            Some("Deleted the override for acme/api."),
            "the delete's confirmation stays"
        );
    }

    fn connected_on_settings() -> App {
        let mut app = App::new("sock");
        app.update(Msg::Auth(Ok(AuthState::Connected {
            login: "octo".into(),
            name: None,
        })));
        app.screen = Screen::Main;
        app.tab = Tab::Settings;
        app
    }

    #[test]
    fn signing_out_from_settings_asks_first() {
        let mut app = connected_on_settings();
        assert!(app.update(key(KeyCode::Char('l'))).is_empty());
        assert!(app.confirm_sign_out);
        assert!(
            app.update(key(KeyCode::Char('n'))).is_empty(),
            "anything else cancels"
        );
        assert!(!app.confirm_sign_out);

        app.update(key(KeyCode::Char('l')));
        assert_eq!(app.update(key(KeyCode::Char('y'))), [Cmd::SignOut]);
        app.update(Msg::Auth(Ok(AuthState::Disconnected)));
        assert_eq!(app.screen, Screen::Login);
    }

    #[test]
    fn a_token_can_replace_the_current_sign_in_from_settings() {
        let mut app = connected_on_settings();
        app.update(key(KeyCode::Char('t')));
        assert_eq!(app.screen, Screen::Login);
        assert!(app.token_input.is_some());
        app.update(key(KeyCode::Esc));
        assert_eq!(
            app.screen,
            Screen::Main,
            "esc goes back while still signed in"
        );
        assert_eq!(app.tab, Tab::Settings);

        app.update(key(KeyCode::Char('t')));
        for c in "ghp_new".chars() {
            app.update(key(KeyCode::Char(c)));
        }
        assert_eq!(
            app.update(key(KeyCode::Enter)),
            [Cmd::SignInWithToken {
                token: "ghp_new".into()
            }]
        );
    }

    #[test]
    fn big_q_stops_the_background_service_after_asking() {
        let mut app = connected_on_settings();
        app.tab = Tab::Queue;
        assert!(app.update(key(KeyCode::Char('Q'))).is_empty());
        assert!(app.confirm_stop_service);
        assert!(
            app.update(key(KeyCode::Char('n'))).is_empty(),
            "anything else keeps it"
        );
        assert!(!app.quit);

        app.update(key(KeyCode::Char('Q')));
        assert_eq!(app.update(key(KeyCode::Char('y'))), [Cmd::StopService]);
        assert!(!app.quit, "quits once it has stopped");
        app.update(Msg::ServiceStopped(Ok(())));
        assert!(app.quit);
    }

    #[test]
    fn a_stopped_service_can_be_started_from_the_not_running_screen() {
        let mut app = App::new("sock");
        app.update(Msg::Connected(Err("connection refused".into())));
        assert_eq!(app.screen, Screen::DaemonDown);
        assert_eq!(app.update(key(KeyCode::Char('s'))), [Cmd::StartService]);
    }

    fn healthy() -> Doctor {
        let mut d = Doctor::default();
        d.claude.tool.found = true;
        d.git.found = true;
        d
    }
}
