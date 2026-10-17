//! The Reviews tab: reviews running now and before, and for each one everything that happened —
//! the context Claude was given, its messages and tool calls, the result, the decision and what
//! was posted to GitHub.

use std::cell::Cell;

use crossterm::event::{KeyCode, KeyEvent};
use prowld::transcripts::TranscriptEntry;
use prowld::{Job, Review};

use crate::app::Cmd;

#[derive(Debug, Clone, Default)]
pub struct ReviewsTab {
    pub jobs: Vec<Job>,
    /// Stored reviews, for each job's decision and cost.
    pub reviews: Vec<Review>,
    pub loaded: bool,
    pub selected: usize,
    pub open: Option<OpenReview>,
    /// A jobs refresh is in flight / another one was asked for meanwhile.
    pub loading: bool,
    pub stale: bool,
}

/// The review being looked at.
#[derive(Debug, Clone, Default)]
pub struct OpenReview {
    pub job_id: i64,
    pub transcript: Vec<TranscriptEntry>,
    pub loaded: bool,
    /// First line shown; ignored while following.
    pub scroll: usize,
    /// Stay at the end as new entries arrive (on while a review runs, until you scroll up).
    pub follow: bool,
    /// The furthest `scroll` can go, as of the last draw.
    pub max_scroll: Cell<usize>,
    /// Lines that fit, as of the last draw (for page up/down).
    pub page: Cell<usize>,
    pub loading: bool,
    pub stale: bool,
}

impl ReviewsTab {
    /// Jobs in display order: running first, then waiting, then the rest, newest first.
    pub fn ordered(&self) -> Vec<&Job> {
        let rank = |job: &Job| match job.status.as_str() {
            "running" => 0,
            "queued" | "interrupted" => 1,
            _ => 2,
        };
        let mut jobs: Vec<&Job> = self.jobs.iter().filter(|j| j.id > 0).collect();
        jobs.sort_by_key(|j| (rank(j), -j.id));
        jobs
    }

    pub fn job(&self, id: i64) -> Option<&Job> {
        self.jobs.iter().find(|j| j.id == id)
    }

    /// The newest stored review of a job.
    pub fn review_of(&self, job_id: i64) -> Option<&Review> {
        self.reviews
            .iter()
            .filter(|r| r.job_id == job_id)
            .max_by_key(|r| r.id)
    }

    /// Asks for the job list, unless a request is already on its way (then once more after).
    pub fn refresh(&mut self) -> Vec<Cmd> {
        if self.loading {
            self.stale = true;
            return vec![];
        }
        self.loading = true;
        vec![Cmd::LoadJobs, Cmd::LoadReviews]
    }

    pub fn on_jobs(&mut self, jobs: Vec<Job>) -> Vec<Cmd> {
        let keep = self.ordered().get(self.selected).map(|j| j.id);
        self.jobs = jobs;
        self.loaded = true;
        self.loading = false;
        self.selected = keep
            .and_then(|id| self.ordered().iter().position(|j| j.id == id))
            .unwrap_or(0);
        if std::mem::take(&mut self.stale) {
            return self.refresh();
        }
        vec![]
    }

    pub fn on_jobs_failed(&mut self) {
        self.loading = false;
        self.stale = false;
    }

    /// Reloads the open transcript, coalescing bursts of updates into one request at a time.
    pub fn refresh_transcript(&mut self) -> Vec<Cmd> {
        let Some(open) = &mut self.open else {
            return vec![];
        };
        if open.loading {
            open.stale = true;
            return vec![];
        }
        open.loading = true;
        vec![Cmd::LoadTranscript {
            job_id: open.job_id,
        }]
    }

    pub fn on_transcript(&mut self, job_id: i64, entries: Vec<TranscriptEntry>) -> Vec<Cmd> {
        let Some(open) = self.open.as_mut().filter(|o| o.job_id == job_id) else {
            return vec![];
        };
        open.transcript = entries;
        open.loaded = true;
        open.loading = false;
        if std::mem::take(&mut open.stale) {
            return self.refresh_transcript();
        }
        vec![]
    }

    pub fn on_transcript_failed(&mut self, job_id: i64) {
        if let Some(open) = self.open.as_mut().filter(|o| o.job_id == job_id) {
            open.loading = false;
            open.stale = false;
        }
    }

    /// Reacts to a daemon event: refreshes the list, and the open transcript if it's about it.
    pub fn on_event(&mut self, kind: &str, job_id: Option<i64>) -> Vec<Cmd> {
        let mut cmds = Vec::new();
        let about_jobs = kind.starts_with("review.")
            || kind.starts_with("job.")
            || kind == "pr.enqueued"
            || kind == "claude.missing";
        if !about_jobs {
            return cmds;
        }
        if self.loaded {
            cmds.extend(self.refresh());
        }
        if job_id.is_some() && self.open.as_ref().map(|o| o.job_id) == job_id {
            cmds.extend(self.refresh_transcript());
        }
        cmds
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Vec<Cmd> {
        if self.open.is_some() {
            return self.on_detail_key(key);
        }
        let len = self.ordered().len();
        match key.code {
            KeyCode::Down | KeyCode::Char('j') if self.selected + 1 < len => self.selected += 1,
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Enter => {
                let Some(job) = self.ordered().get(self.selected).map(|j| (*j).clone()) else {
                    return vec![];
                };
                self.open = Some(OpenReview {
                    job_id: job.id,
                    follow: matches!(job.status.as_str(), "running" | "queued" | "interrupted"),
                    ..Default::default()
                });
                return self.refresh_transcript();
            }
            KeyCode::Char('o') => return self.open_on_github(None),
            KeyCode::Char('R') => return self.rerun(None),
            KeyCode::Char('v') => return self.verify(None),
            KeyCode::Char('r') => return self.refresh(),
            _ => {}
        }
        vec![]
    }

    fn on_detail_key(&mut self, key: KeyEvent) -> Vec<Cmd> {
        let Some(open) = &mut self.open else {
            return vec![];
        };
        let max = open.max_scroll.get();
        let page = open.page.get().max(1);
        let current = if open.follow {
            max
        } else {
            open.scroll.min(max)
        };
        let go = |to: usize, open: &mut OpenReview| {
            open.scroll = to.min(max);
            open.follow = to >= max;
        };
        match key.code {
            KeyCode::Esc | KeyCode::Backspace => self.open = None,
            KeyCode::Down | KeyCode::Char('j') => go(current + 1, open),
            KeyCode::Up | KeyCode::Char('k') => go(current.saturating_sub(1), open),
            KeyCode::PageDown | KeyCode::Char(' ') | KeyCode::Char('n') => go(current + page, open),
            KeyCode::PageUp | KeyCode::Char('p') => go(current.saturating_sub(page), open),
            KeyCode::Home | KeyCode::Char('g') => go(0, open),
            KeyCode::End | KeyCode::Char('G') => go(max, open),
            KeyCode::Char('r') => return self.refresh_transcript(),
            KeyCode::Char('o') => {
                let job_id = open.job_id;
                return self.open_on_github(Some(job_id));
            }
            KeyCode::Char('R') => {
                let job_id = open.job_id;
                return self.rerun(Some(job_id));
            }
            KeyCode::Char('v') => {
                let job_id = open.job_id;
                return self.verify(Some(job_id));
            }
            _ => {}
        }
        vec![]
    }

    /// Reviews the selected (or open) job's PR again.
    fn rerun(&self, job_id: Option<i64>) -> Vec<Cmd> {
        let job = match job_id {
            Some(id) => self.job(id),
            None => self.ordered().get(self.selected).copied(),
        };
        job.map(|j| {
            vec![Cmd::Rerun {
                repo: j.repo.clone(),
                pr: j.pr_number,
            }]
        })
        .unwrap_or_default()
    }

    /// Checks the selected (or open) job's PR for addressed findings, approving it if so.
    fn verify(&self, job_id: Option<i64>) -> Vec<Cmd> {
        let job = match job_id {
            Some(id) => self.job(id),
            None => self.ordered().get(self.selected).copied(),
        };
        job.map(|j| {
            vec![Cmd::Verify {
                repo: j.repo.clone(),
                pr: j.pr_number,
            }]
        })
        .unwrap_or_default()
    }

    /// Opens what PRowl posted, or else the PR.
    fn open_on_github(&self, job_id: Option<i64>) -> Vec<Cmd> {
        let job = match job_id {
            Some(id) => self.job(id),
            None => self.ordered().get(self.selected).copied(),
        };
        let Some(job) = job else {
            return vec![];
        };
        let url = self
            .review_of(job.id)
            .and_then(|r| r.published_url.clone())
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| job.html_url.clone());
        if url.is_empty() {
            vec![]
        } else {
            vec![Cmd::OpenUrl { url }]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn job(id: i64, status: &str) -> Job {
        Job {
            id,
            repo: "acme/api".into(),
            pr_number: id as u64,
            head_sha: "abc".into(),
            title: format!("PR {id}"),
            author: "dev".into(),
            html_url: format!("https://github.com/acme/api/pull/{id}"),
            status: status.into(),
            attempts: 1,
            error: None,
            created_at_ms: 0,
            kind: "review".into(),
        }
    }

    #[test]
    fn running_reviews_come_first_then_waiting_then_newest() {
        let mut tab = ReviewsTab::default();
        tab.on_jobs(vec![
            job(1, "done"),
            job(2, "queued"),
            job(3, "running"),
            job(4, "failed"),
            job(-5, "dry_run"),
        ]);
        let order: Vec<i64> = tab.ordered().iter().map(|j| j.id).collect();
        assert_eq!(order, [3, 2, 4, 1], "dry runs aren't listed");
    }

    #[test]
    fn opening_a_running_review_follows_it_and_events_refresh_it() {
        let mut tab = ReviewsTab::default();
        tab.on_jobs(vec![job(3, "running"), job(1, "done")]);

        assert_eq!(
            tab.on_key(key(KeyCode::Enter)),
            [Cmd::LoadTranscript { job_id: 3 }]
        );
        assert!(tab.open.as_ref().unwrap().follow);
        // A burst of output while the first load is in flight asks again only once.
        assert_eq!(
            tab.on_event("review.output", Some(3)),
            [Cmd::LoadJobs, Cmd::LoadReviews]
        );
        assert!(tab.on_event("review.tool", Some(3)).is_empty());
        assert_eq!(
            tab.on_transcript(3, vec![]),
            [Cmd::LoadTranscript { job_id: 3 }]
        );
        assert!(
            tab.on_event("review.output", Some(1)).is_empty(),
            "other jobs don't reload this transcript (and the list is already loading)"
        );
    }

    #[test]
    fn scrolling_up_stops_following_and_end_resumes() {
        let mut tab = ReviewsTab::default();
        tab.on_jobs(vec![job(3, "running")]);
        tab.on_key(key(KeyCode::Enter));
        let open = tab.open.as_ref().unwrap();
        open.max_scroll.set(50);
        open.page.set(10);

        tab.on_key(key(KeyCode::Up));
        let open = tab.open.as_ref().unwrap();
        assert!(!open.follow);
        assert_eq!(open.scroll, 49);
        tab.on_key(key(KeyCode::PageUp));
        assert_eq!(tab.open.as_ref().unwrap().scroll, 39);
        tab.on_key(key(KeyCode::Char('G')));
        assert!(tab.open.as_ref().unwrap().follow);
        tab.on_key(key(KeyCode::Esc));
        assert!(tab.open.is_none());
    }

    #[test]
    fn o_opens_what_was_posted_or_the_pr() {
        let mut tab = ReviewsTab::default();
        tab.on_jobs(vec![job(1, "done")]);
        assert_eq!(
            tab.on_key(key(KeyCode::Char('o'))),
            [Cmd::OpenUrl {
                url: "https://github.com/acme/api/pull/1".into()
            }]
        );
    }
}
