use std::path::{Path, PathBuf};
use std::process::Command;

/// Bare repositories on disk standing in for github.com, with real commits and PR refs.
pub struct GitRemote {
    root: PathBuf,
}

/// The commits a scripted pull request was built from.
#[derive(Debug, Clone)]
pub struct PrCommits {
    pub base_sha: String,
    pub head_sha: String,
}

impl GitRemote {
    pub fn new(root: PathBuf) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// The base the daemon clones from: repositories live at `<base>/<owner>/<repo>.git`.
    pub fn base(&self) -> String {
        self.root.display().to_string().replace('\\', "/")
    }

    /// Creates `repo` with `base_files` on `main`, then a PR `number` adding `head_files`,
    /// published as `refs/pull/<number>/head` like GitHub does.
    pub fn create_pr(
        &self,
        repo: &str,
        number: u64,
        base_files: &[(&str, &str)],
        head_files: &[(&str, &str)],
    ) -> anyhow::Result<PrCommits> {
        let bare = self.root.join(format!("{repo}.git"));
        if !bare.exists() {
            std::fs::create_dir_all(&bare)?;
            git(
                &bare,
                &["init", "--bare", "--quiet", "--initial-branch=main", "."],
            )?;
        }
        let work = tempfile::tempdir()?;
        let work = work.path();
        git(work, &["init", "--quiet", "--initial-branch=main", "."])?;
        git(
            work,
            &["remote", "add", "origin", &bare.display().to_string()],
        )?;
        write_files(work, base_files)?;
        commit(work, "base")?;
        git(
            work,
            &[
                "push",
                "--quiet",
                "--force",
                "origin",
                "HEAD:refs/heads/main",
            ],
        )?;
        let base_sha = git(work, &["rev-parse", "HEAD"])?;
        write_files(work, head_files)?;
        commit(work, &format!("PR {number}"))?;
        git(
            work,
            &[
                "push",
                "--quiet",
                "--force",
                "origin",
                &format!("HEAD:refs/pull/{number}/head"),
            ],
        )?;
        let head_sha = git(work, &["rev-parse", "HEAD"])?;
        Ok(PrCommits { base_sha, head_sha })
    }
}

fn write_files(dir: &Path, files: &[(&str, &str)]) -> anyhow::Result<()> {
    for (path, contents) in files {
        let path = dir.join(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, contents)?;
    }
    Ok(())
}

fn commit(dir: &Path, message: &str) -> anyhow::Result<()> {
    git(dir, &["add", "--all"])?;
    git(
        dir,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            message,
        ],
    )?;
    Ok(())
}

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = Command::new("git").arg("-C").arg(dir).args(args).output()?;
    anyhow::ensure!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
