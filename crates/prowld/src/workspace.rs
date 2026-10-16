//! Git checkouts for reviews: one cached bare clone per repository, and a throwaway worktree
//! per job at the PR's head commit. Reviews never touch the user's own working copies.
//!
//! Claude gets no shell, so the diff and commit list are computed here and written into the
//! checkout under [`REVIEW_DIR`], where its read-only file tools can reach them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use tokio::process::Command;
use tokio::sync::Mutex;

/// Directory inside the checkout holding PRowl's precomputed review material.
pub(crate) const REVIEW_DIR: &str = ".prowl-review";

#[derive(Clone)]
pub(crate) struct Workspace {
    repos_dir: PathBuf,
    worktrees_dir: PathBuf,
    /// Base for clone URLs: `https://github.com`, or a local directory in tests.
    git_base: String,
    /// One lock per repository, so fetches and worktree changes don't collide.
    locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

impl Workspace {
    pub(crate) fn new(home: &Path, git_base: &str) -> Self {
        Self {
            repos_dir: home.join("git"),
            // Short path: Windows tooling still trips over long ones.
            worktrees_dir: home.join("wt"),
            git_base: git_base.trim_end_matches('/').to_owned(),
            locks: Arc::default(),
        }
    }

    /// Fetches the PR and its base branch, checks out `head_sha` in a fresh worktree, and
    /// writes the diff against `base_sha` (merge-base form) and the commit list into it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn checkout(
        &self,
        job_id: i64,
        repo: &str,
        pr_number: u64,
        base_ref: &str,
        base_sha: &str,
        head_sha: &str,
        token: &str,
    ) -> anyhow::Result<PathBuf> {
        let lock = self.lock_for(repo).await;
        let _guard = lock.lock().await;
        let bare = self.ensure_bare(repo).await?;
        let auth = auth_env(&self.git_base, token);
        git(&bare, &auth)
            .args(["fetch", "--no-tags", "--force", "origin"])
            .arg(format!(
                "+refs/pull/{pr_number}/head:refs/prowl/pull/{pr_number}"
            ))
            .arg(format!("+refs/heads/{base_ref}:refs/prowl/base/{base_ref}"))
            .run()
            .await
            .with_context(|| format!("fetching {repo}#{pr_number}"))?;

        let worktree = self.worktrees_dir.join(job_id.to_string());
        if worktree.exists() {
            self.remove_locked(&bare, &worktree).await;
        }
        std::fs::create_dir_all(&self.worktrees_dir)?;
        git(&bare, &[])
            .args(["worktree", "add", "--detach"])
            .arg(&worktree)
            .arg(head_sha)
            .run()
            .await
            .with_context(|| format!("checking out {head_sha}"))?;
        write_review_material(&worktree, base_sha).await?;
        Ok(worktree)
    }

    pub(crate) async fn remove(&self, repo: &str, worktree: &Path) {
        let lock = self.lock_for(repo).await;
        let _guard = lock.lock().await;
        self.remove_locked(&self.bare_path(repo), worktree).await;
    }

    /// Removes worktrees left behind by a crash.
    pub(crate) async fn remove_all_worktrees(&self) {
        let Ok(entries) = std::fs::read_dir(&self.worktrees_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let _ = std::fs::remove_dir_all(entry.path());
        }
        if let Ok(owners) = std::fs::read_dir(&self.repos_dir) {
            for owner in owners.flatten() {
                for repo in std::fs::read_dir(owner.path())
                    .into_iter()
                    .flatten()
                    .flatten()
                {
                    let _ = git(&repo.path(), &[])
                        .args(["worktree", "prune"])
                        .run()
                        .await;
                }
            }
        }
    }

    async fn remove_locked(&self, bare: &Path, worktree: &Path) {
        let removed = git(bare, &[])
            .args(["worktree", "remove", "--force"])
            .arg(worktree)
            .run()
            .await;
        if removed.is_err() {
            let _ = std::fs::remove_dir_all(worktree);
            let _ = git(bare, &[]).args(["worktree", "prune"]).run().await;
        }
    }

    async fn ensure_bare(&self, repo: &str) -> anyhow::Result<PathBuf> {
        let bare = self.bare_path(repo);
        if bare.join("HEAD").exists() {
            return Ok(bare);
        }
        std::fs::create_dir_all(&bare)?;
        Git::new(&bare, &[])
            .args(["init", "--bare", "--quiet", "."])
            .run()
            .await?;
        for (key, value) in [
            ("core.longpaths", "true"),
            ("core.autocrlf", "false"),
            ("gc.auto", "0"),
        ] {
            git(&bare, &[]).args(["config", key, value]).run().await?;
        }
        git(&bare, &[])
            .args(["remote", "add", "origin"])
            .arg(format!("{}/{repo}.git", self.git_base))
            .run()
            .await?;
        Ok(bare)
    }

    fn bare_path(&self, repo: &str) -> PathBuf {
        let (owner, name) = repo.split_once('/').unwrap_or(("_", repo));
        self.repos_dir.join(owner).join(format!("{name}.git"))
    }

    async fn lock_for(&self, repo: &str) -> Arc<Mutex<()>> {
        self.locks
            .lock()
            .await
            .entry(repo.to_owned())
            .or_default()
            .clone()
    }
}

async fn write_review_material(worktree: &Path, base_sha: &str) -> anyhow::Result<()> {
    let range = format!("{base_sha}...HEAD");
    // No external diff drivers or text conversion: only git's own output.
    let diff = git(worktree, &[])
        .args([
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--find-renames",
        ])
        .arg(&range)
        .run()
        .await
        .context("computing the PR diff")?;
    let commits = git(worktree, &[])
        .args([
            "log",
            "--no-color",
            "--format=%h %an: %s%n%n%b",
            "--no-merges",
        ])
        .arg(format!("{base_sha}..HEAD"))
        .run()
        .await
        .context("listing the PR's commits")?;
    let dir = worktree.join(REVIEW_DIR);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("pr.diff"), diff)?;
    std::fs::write(dir.join("commits.txt"), commits)?;
    Ok(())
}

/// HTTPS credentials passed through git's environment, so the token never lands in a config
/// file or on a command line.
fn auth_env(git_base: &str, token: &str) -> Vec<(String, String)> {
    if !git_base.starts_with("https://") {
        return Vec::new();
    }
    let basic = base64_encode(format!("x-access-token:{token}").as_bytes());
    vec![
        ("GIT_CONFIG_COUNT".into(), "1".into()),
        ("GIT_CONFIG_KEY_0".into(), "http.extraHeader".into()),
        (
            "GIT_CONFIG_VALUE_0".into(),
            format!("Authorization: Basic {basic}"),
        ),
    ]
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn git(dir: &Path, env: &[(String, String)]) -> Git {
    Git::new(dir, env)
}

struct Git(Command);

impl Git {
    fn new(dir: &Path, env: &[(String, String)]) -> Self {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(dir);
        cmd.envs(env.iter().map(|(k, v)| (k, v)));
        // Never prompt for credentials; fail instead.
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        cmd.kill_on_drop(true);
        Self(cmd)
    }

    fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        self.0.args(args);
        self
    }

    fn arg(mut self, arg: impl AsRef<std::ffi::OsStr>) -> Self {
        self.0.arg(arg);
        self
    }

    async fn run(mut self) -> anyhow::Result<String> {
        let output = self
            .0
            .output()
            .await
            .context("running git (is it installed?)")?;
        if !output.status.success() {
            bail!(
                "git failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64_encode(input.as_bytes()), expected);
        }
    }

    #[test]
    fn credentials_only_go_to_https_remotes() {
        assert!(auth_env("/tmp/remotes", "t").is_empty());
        let env = auth_env("https://github.com", "t");
        assert_eq!(env[2].1, "Authorization: Basic eC1hY2Nlc3MtdG9rZW46dA==");
    }
}
