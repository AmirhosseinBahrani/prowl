//! Finds the external tools PRowl runs. A background service starts with a minimal PATH, so
//! after PATH, the usual install locations are checked too.

use std::path::{Path, PathBuf};

/// `name` on `PATH`, else in one of the usual install locations for it.
pub(crate) fn find(name: &str) -> Option<PathBuf> {
    let home = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf());
    find_in(
        name,
        std::env::var_os("PATH"),
        &candidates(name, home.as_deref()),
    )
}

fn find_in(
    name: &str,
    path: Option<std::ffi::OsString>,
    candidates: &[PathBuf],
) -> Option<PathBuf> {
    let cwd = std::env::current_dir().unwrap_or_default();
    // `which` also resolves `claude.exe` / `claude.cmd` on Windows.
    which::which_in(name, path, cwd)
        .ok()
        .or_else(|| candidates.iter().find(|c| is_executable(c)).cloned())
}

/// Where installers put `name` when it isn't on the service's PATH.
fn candidates(name: &str, home: Option<&Path>) -> Vec<PathBuf> {
    let exe = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(home) = home {
        dirs.extend(
            [
                ".local/bin",      // Claude Code's native installer
                ".claude/local",   // Claude Code's older local install
                ".npm-global/bin", // npm with a user prefix
                ".bun/bin",
                ".volta/bin",
                ".cargo/bin",
            ]
            .map(|d| home.join(d)),
        );
    }
    if cfg!(unix) {
        dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].map(PathBuf::from));
    }
    dirs.into_iter().map(|d| d.join(&exe)).collect()
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tool(dir: &Path, name: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    #[test]
    fn a_tool_missing_from_path_is_found_where_installers_put_it() {
        let home = tempfile::tempdir().unwrap();
        let installed = make_tool(&home.path().join(".local/bin"), "prowl-test-tool");
        let minimal_path = Some(std::ffi::OsString::from("/nonexistent"));

        let found = find_in(
            "prowl-test-tool",
            minimal_path,
            &candidates("prowl-test-tool", Some(home.path())),
        );

        assert_eq!(found, Some(installed));
    }

    #[test]
    fn path_wins_over_install_locations() {
        let home = tempfile::tempdir().unwrap();
        make_tool(&home.path().join(".local/bin"), "prowl-test-tool");
        let on_path = make_tool(&home.path().join("on-path"), "prowl-test-tool");

        let found = find_in(
            "prowl-test-tool",
            Some(home.path().join("on-path").into_os_string()),
            &candidates("prowl-test-tool", Some(home.path())),
        );

        assert_eq!(found, Some(on_path));
    }

    #[test]
    fn nothing_is_found_when_the_tool_is_nowhere() {
        let home = tempfile::tempdir().unwrap();
        let found = find_in(
            "prowl-test-tool",
            Some("/nonexistent".into()),
            &candidates("prowl-test-tool", Some(home.path())),
        );
        assert_eq!(found, None);
    }
}
