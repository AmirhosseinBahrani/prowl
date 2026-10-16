//! Stand-in for the `claude` CLI in tests. Reads scripted responses from
//! `$PROWL_FAKE_CLAUDE_DIR/responses.json` and records each call under `invocations/`.

use std::io::{Read, Write};
use std::path::PathBuf;

use prowl_e2e::{ClaudeInvocation, ClaudeResponse};

fn main() {
    let dir: PathBuf = std::env::var_os("PROWL_FAKE_CLAUDE_DIR")
        .expect("PROWL_FAKE_CLAUDE_DIR must be set")
        .into();

    let mut stdin = String::new();
    // stdin may be closed or a terminal; either way, an empty prompt is fine.
    let _ = std::io::stdin().read_to_string(&mut stdin);

    let cwd = std::env::current_dir().unwrap_or_default();
    let mut files: Vec<String> = std::fs::read_dir(&cwd)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    let git_head = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());
    let args: Vec<String> = std::env::args().skip(1).collect();
    let extra_dir = args
        .iter()
        .position(|a| a == "--add-dir")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from);
    let read = |name: &str| {
        extra_dir
            .as_ref()
            .and_then(|d| std::fs::read_to_string(d.join(name)).ok())
    };
    let review_diff = read("pr.diff");
    let review_commits = read("commits.txt");
    let invocation = ClaudeInvocation {
        args,
        stdin,
        cwd,
        files,
        git_head,
        review_diff,
        review_commits,
    };
    let n = record(&dir, &invocation);

    let responses: Vec<ClaudeResponse> = std::fs::read(dir.join("responses.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let response = responses
        .get(n)
        .or(responses.last())
        .cloned()
        .unwrap_or_default();

    if response.delay_ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(response.delay_ms));
    }
    let _ = std::io::stdout().write_all(response.stdout.as_bytes());
    let _ = std::io::stderr().write_all(response.stderr.as_bytes());
    std::process::exit(response.exit_code);
}

/// Writes the invocation to the first free slot and returns its index.
fn record(dir: &std::path::Path, invocation: &ClaudeInvocation) -> usize {
    let body = serde_json::to_vec_pretty(invocation).expect("serialize invocation");
    for n in 0.. {
        let path = dir.join("invocations").join(format!("{n:06}.json"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                f.write_all(&body).expect("write invocation");
                return n;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => panic!("cannot record invocation: {e}"),
        }
    }
    unreachable!()
}
