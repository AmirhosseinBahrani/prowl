//! Git checkouts for reviews: one cached bare clone per repository, and a throwaway worktree
//! per job at the PR's head commit. Reviews never touch the user's own working copies.
//!
//! Claude gets no shell, so the diff and commit list are computed here and written to a
//! directory PRowl owns, outside the checkout ([`Checkout::material`]). Nothing is ever written
//! inside the checkout: its contents come from the PR, which could plant symlinks there.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use tokio::process::Command;
use tokio::sync::Mutex;

/// A PR checked out for review.
#[derive(Debug, Clone)]
pub(crate) struct Checkout {
    /// The PR's files at its head commit. Untrusted: written only by git.
    pub worktree: PathBuf,
    /// PRowl's precomputed `pr.diff` and `commits.txt`, outside the worktree.
    pub material: PathBuf,
}

#[derive(Clone)]
pub(crate) struct Workspace {
    repos_dir: PathBuf,
    worktrees_dir: PathBuf,
    material_dir: PathBuf,
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
            material_dir: home.join("review"),
            git_base: git_base.trim_end_matches('/').to_owned(),
            locks: Arc::default(),
        }
    }

    /// Fetches the PR and its base branch, checks out `head_sha` in a fresh worktree, and
    /// writes the diff against `base_sha` (merge-base form) and the commit list beside it.
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
    ) -> anyhow::Result<Checkout> {
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
        let material = self.material_dir.join(job_id.to_string());
        write_review_material(&worktree, &material, base_sha).await?;
        Ok(Checkout { worktree, material })
    }

    pub(crate) async fn remove(&self, repo: &str, checkout: &Checkout) {
        let lock = self.lock_for(repo).await;
        let _guard = lock.lock().await;
        self.remove_locked(&self.bare_path(repo), &checkout.worktree)
            .await;
        let _ = std::fs::remove_dir_all(&checkout.material);
    }

    /// Removes worktrees left behind by a crash.
    /// Cleans up after a crash: removes leftover checkouts and review material, and the lock
    /// files git leaves when it is killed mid-operation. Only call it when no PRowl git
    /// process can be running (at startup).
    pub(crate) async fn recover(&self) {
        let _ = std::fs::remove_dir_all(&self.material_dir);
        if let Ok(entries) = std::fs::read_dir(&self.worktrees_dir) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
        for repo in self.bare_repos() {
            remove_lock_files(&repo);
            let _ = git(&repo, &[]).args(["worktree", "prune"]).run().await;
        }
    }

    fn bare_repos(&self) -> Vec<PathBuf> {
        std::fs::read_dir(&self.repos_dir)
            .into_iter()
            .flatten()
            .flatten()
            .flat_map(|owner| {
                std::fs::read_dir(owner.path())
                    .into_iter()
                    .flatten()
                    .flatten()
            })
            .map(|repo| repo.path())
            .filter(|path| path.extension().is_some_and(|e| e == "git"))
            .collect()
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
        let url = format!("{}/{repo}.git", self.git_base);
        if bare.join("HEAD").exists() {
            // Repair a clone whose remote is missing (an older interrupted setup) or stale.
            let current = git(&bare, &[])
                .args(["remote", "get-url", "origin"])
                .run()
                .await;
            match current {
                Ok(current) if current.trim() == url => {}
                Ok(_) => {
                    git(&bare, &[])
                        .args(["remote", "set-url", "origin"])
                        .arg(&url)
                        .run()
                        .await?;
                }
                Err(_) => {
                    git(&bare, &[])
                        .args(["remote", "add", "origin"])
                        .arg(&url)
                        .run()
                        .await?;
                }
            }
            return Ok(bare);
        }
        // Build the clone beside its final path and rename it into place, so an interrupted
        // setup never leaves a half-configured repository behind.
        let staging = bare.with_extension(format!("git.init-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging)?;
        Git::new(&staging, &[])
            .args(["init", "--bare", "--quiet", "."])
            .run()
            .await?;
        for (key, value) in [
            ("core.longpaths", "true"),
            ("core.autocrlf", "false"),
            ("gc.auto", "0"),
        ] {
            git(&staging, &[])
                .args(["config", key, value])
                .run()
                .await?;
        }
        git(&staging, &[])
            .args(["remote", "add", "origin"])
            .arg(&url)
            .run()
            .await?;
        let _ = std::fs::remove_dir_all(&bare);
        std::fs::rename(&staging, &bare)
            .with_context(|| format!("moving the new clone into {}", bare.display()))?;
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

async fn write_review_material(
    worktree: &Path,
    material: &Path,
    base_sha: &str,
) -> anyhow::Result<()> {
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
    // `material` is PRowl's own path (home/review/<job>), never inside the checkout.
    let _ = std::fs::remove_dir_all(material);
    std::fs::create_dir_all(material)?;
    write_new(&material.join("pr.diff"), &diff)?;
    write_new(&material.join("commits.txt"), &commits)?;
    Ok(())
}

/// Creates `path`, refusing to reuse anything already there (including a symlink).
fn write_new(path: &Path, contents: &str) -> anyhow::Result<()> {
    use std::io::Write;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .and_then(|mut f| f.write_all(contents.as_bytes()))
        .with_context(|| format!("writing {}", path.display()))
}

/// Removes `*.lock` files under a bare repository (git never uses that suffix for real refs).
fn remove_lock_files(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() && !t.is_symlink() => remove_lock_files(&path),
            Ok(t) if t.is_file() && path.extension().is_some_and(|e| e == "lock") => {
                tracing::info!(path = %path.display(), "removing stale git lock");
                let _ = std::fs::remove_file(&path);
            }
            _ => {}
        }
    }
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
