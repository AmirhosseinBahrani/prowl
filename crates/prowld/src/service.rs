//! Installs `prowld-service` as a per-user background service that starts at login and is
//! restarted if it dies: a launchd agent (macOS), a systemd user unit (Linux) or a Task
//! Scheduler logon task (Windows).
//!
//! It runs as the logged-in user rather than as a system service, because the `claude` CLI
//! login and the GitHub token belong to that user.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, anyhow, bail};
use serde::{Deserialize, Serialize};

/// Name of the headless daemon binary the service runs (no console window on Windows).
pub const SERVICE_BIN: &str = "prowld-service";
pub const DEFAULT_NAME: &str = "prowld";

#[derive(Debug, Clone)]
pub struct ServiceSpec {
    /// Distinguishes installs; the default is `prowld`.
    pub name: String,
    /// Absolute path of the `prowld-service` executable.
    pub exe: PathBuf,
    /// PRowl home the service runs with.
    pub home: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceStatus {
    pub installed: bool,
    pub running: bool,
    pub definition: Option<PathBuf>,
}

impl ServiceSpec {
    /// A spec for the `prowld-service` binary installed next to the current executable.
    pub fn beside_current_exe(name: &str, home: &Path) -> anyhow::Result<Self> {
        let current = std::env::current_exe().context("locating the prowld executable")?;
        let exe = current
            .parent()
            .ok_or_else(|| anyhow!("prowld has no parent directory"))?
            .join(format!("{SERVICE_BIN}{}", std::env::consts::EXE_SUFFIX));
        if !exe.exists() {
            bail!(
                "{} not found; it must be installed next to prowld",
                exe.display()
            );
        }
        let home = std::path::absolute(home).context("resolving the PRowl home")?;
        Ok(Self {
            name: name.to_owned(),
            exe,
            home,
        })
    }

    fn label(&self) -> String {
        format!("dev.prowl.{}", self.name)
    }

    fn log_file(&self) -> PathBuf {
        self.home.join("logs").join(format!("{}.log", self.name))
    }
}

pub fn install(spec: &ServiceSpec) -> anyhow::Result<ServiceStatus> {
    if let Some(folder) = protected_folder(&spec.exe) {
        bail!(
            "{} is inside ~/{folder}, which macOS does not let background services read; \
             install PRowl somewhere else (e.g. /Applications or ~/.local/bin) and run this again",
            spec.exe.display()
        );
    }
    std::fs::create_dir_all(spec.home.join("logs"))?;
    imp::install(spec)?;
    // Give the service a moment to come up so the reported status is meaningful.
    for _ in 0..30 {
        let current = status(spec)?;
        if current.running {
            return Ok(current);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    status(spec)
}

/// The macOS privacy-protected home folder `exe` lives in, if any.
fn protected_folder(exe: &Path) -> Option<&'static str> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let home = directories::BaseDirs::new()?.home_dir().to_path_buf();
    ["Desktop", "Documents", "Downloads"]
        .into_iter()
        .find(|folder| exe.starts_with(home.join(folder)))
}

pub fn uninstall(spec: &ServiceSpec) -> anyhow::Result<ServiceStatus> {
    imp::uninstall(spec)?;
    status(spec)
}

pub fn status(spec: &ServiceSpec) -> anyhow::Result<ServiceStatus> {
    imp::status(spec)
}

// ---- Service definitions. Rendered on every OS so they can be unit-tested anywhere. ----

pub fn launchd_plist(spec: &ServiceSpec) -> String {
    let arg = |s: &str| format!("        <string>{}</string>\n", xml_escape(s));
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
{exe}{flag}{home}    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ThrottleInterval</key>
    <integer>10</integer>
    <key>ProcessType</key>
    <string>Background</string>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
        label = xml_escape(&spec.label()),
        exe = arg(&spec.exe.display().to_string()),
        flag = arg("--home"),
        home = arg(&spec.home.display().to_string()),
        log = xml_escape(&spec.log_file().display().to_string()),
    )
}

pub fn systemd_unit(spec: &ServiceSpec) -> String {
    format!(
        "[Unit]
Description=PRowl pull-request review daemon ({name})
After=network-online.target

[Service]
Type=simple
ExecStart={exe} --home {home}
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
",
        name = spec.name,
        exe = systemd_quote(&spec.exe.display().to_string()),
        home = systemd_quote(&spec.home.display().to_string()),
    )
}

/// `user` is `DOMAIN\name`.
pub fn task_scheduler_xml(spec: &ServiceSpec, user: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>PRowl pull-request review daemon ({name})</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <StartWhenAvailable>true</StartWhenAvailable>
    <Hidden>true</Hidden>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>999</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe}</Command>
      <Arguments>--home "{home}"</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
        name = xml_escape(&spec.name),
        user = xml_escape(user),
        exe = xml_escape(&spec.exe.display().to_string()),
        home = xml_escape(&spec.home.display().to_string()),
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Quotes one argument for a systemd `ExecStart=` line.
fn systemd_quote(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
        .replace('$', "$$");
    format!("\"{escaped}\"")
}

/// Runs a command, failing with its stderr if it exits non-zero.
fn run(cmd: &mut Command) -> anyhow::Result<String> {
    let output = cmd
        .output()
        .with_context(|| format!("running {:?}", cmd.get_program()))?;
    if !output.status.success() {
        bail!(
            "{:?} failed: {}",
            cmd.get_program(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;

    fn plist_path(spec: &ServiceSpec) -> anyhow::Result<PathBuf> {
        let home = directories::BaseDirs::new()
            .ok_or_else(|| anyhow!("no home directory"))?
            .home_dir()
            .to_path_buf();
        Ok(home
            .join("Library/LaunchAgents")
            .join(format!("{}.plist", spec.label())))
    }

    fn domain() -> anyhow::Result<String> {
        Ok(format!("gui/{}", run(Command::new("id").arg("-u"))?.trim()))
    }

    pub fn install(spec: &ServiceSpec) -> anyhow::Result<()> {
        let path = plist_path(spec)?;
        std::fs::create_dir_all(path.parent().expect("has parent"))?;
        // Replace any previous definition so the new one takes effect.
        let _ = run(Command::new("launchctl").arg("bootout").arg(format!(
            "{}/{}",
            domain()?,
            spec.label()
        )));
        std::fs::write(&path, launchd_plist(spec))?;
        run(Command::new("launchctl")
            .arg("bootstrap")
            .arg(domain()?)
            .arg(&path))?;
        Ok(())
    }

    pub fn uninstall(spec: &ServiceSpec) -> anyhow::Result<()> {
        let _ = run(Command::new("launchctl").arg("bootout").arg(format!(
            "{}/{}",
            domain()?,
            spec.label()
        )));
        let path = plist_path(spec)?;
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        Ok(())
    }

    pub fn status(spec: &ServiceSpec) -> anyhow::Result<ServiceStatus> {
        let path = plist_path(spec)?;
        let printed = run(Command::new("launchctl").arg("print").arg(format!(
            "{}/{}",
            domain()?,
            spec.label()
        )));
        Ok(ServiceStatus {
            installed: path.exists(),
            running: printed.is_ok_and(|out| out.contains("state = running")),
            definition: Some(path),
        })
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod imp {
    use super::*;

    fn unit_name(spec: &ServiceSpec) -> String {
        format!("{}.service", spec.name)
    }

    fn unit_path(spec: &ServiceSpec) -> anyhow::Result<PathBuf> {
        let config = directories::BaseDirs::new()
            .ok_or_else(|| anyhow!("no home directory"))?
            .config_dir()
            .to_path_buf();
        Ok(config.join("systemd/user").join(unit_name(spec)))
    }

    fn systemctl() -> Command {
        let mut cmd = Command::new("systemctl");
        cmd.arg("--user");
        cmd
    }

    pub fn install(spec: &ServiceSpec) -> anyhow::Result<()> {
        let path = unit_path(spec)?;
        std::fs::create_dir_all(path.parent().expect("has parent"))?;
        std::fs::write(&path, systemd_unit(spec))?;
        run(systemctl().arg("daemon-reload"))?;
        run(systemctl().args(["enable", "--now"]).arg(unit_name(spec)))?;
        // A unit that was already running keeps its old command line until restarted.
        run(systemctl().arg("restart").arg(unit_name(spec)))?;
        Ok(())
    }

    pub fn uninstall(spec: &ServiceSpec) -> anyhow::Result<()> {
        let _ = run(systemctl().args(["disable", "--now"]).arg(unit_name(spec)));
        let path = unit_path(spec)?;
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        run(systemctl().arg("daemon-reload"))?;
        Ok(())
    }

    pub fn status(spec: &ServiceSpec) -> anyhow::Result<ServiceStatus> {
        let path = unit_path(spec)?;
        let active = run(systemctl().arg("is-active").arg(unit_name(spec)));
        Ok(ServiceStatus {
            installed: path.exists(),
            running: active.is_ok_and(|out| out.trim() == "active"),
            definition: Some(path),
        })
    }
}

#[cfg(windows)]
mod imp {
    use super::*;

    fn task_name(spec: &ServiceSpec) -> String {
        format!(r"\PRowl\{}", spec.name)
    }

    fn current_user() -> anyhow::Result<String> {
        let name = std::env::var("USERNAME").context("USERNAME is not set")?;
        Ok(match std::env::var("USERDOMAIN") {
            Ok(domain) => format!(r"{domain}\{name}"),
            Err(_) => name,
        })
    }

    pub fn install(spec: &ServiceSpec) -> anyhow::Result<()> {
        let xml = task_scheduler_xml(spec, &current_user()?);
        let path = spec.home.join(format!("{}-task.xml", spec.name));
        // schtasks expects UTF-16LE with a byte-order mark.
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
        std::fs::write(&path, bytes)?;
        run(Command::new("schtasks")
            .args(["/Create", "/F", "/TN"])
            .arg(task_name(spec))
            .arg("/XML")
            .arg(&path))?;
        let _ = run(Command::new("schtasks")
            .args(["/End", "/TN"])
            .arg(task_name(spec)));
        run(Command::new("schtasks")
            .args(["/Run", "/TN"])
            .arg(task_name(spec)))?;
        Ok(())
    }

    pub fn uninstall(spec: &ServiceSpec) -> anyhow::Result<()> {
        let _ = run(Command::new("schtasks")
            .args(["/End", "/TN"])
            .arg(task_name(spec)));
        let _ = run(Command::new("schtasks")
            .args(["/Delete", "/F", "/TN"])
            .arg(task_name(spec)));
        Ok(())
    }

    pub fn status(spec: &ServiceSpec) -> anyhow::Result<ServiceStatus> {
        let query = run(Command::new("schtasks")
            .args(["/Query", "/FO", "CSV", "/NH", "/TN"])
            .arg(task_name(spec)));
        Ok(ServiceStatus {
            installed: query.is_ok(),
            running: query.is_ok_and(|out| out.contains("Running")),
            definition: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ServiceSpec {
        ServiceSpec {
            name: "prowld".into(),
            exe: PathBuf::from("/opt/Pr Owl/prowld-service"),
            home: PathBuf::from("/Users/a&b/Library/Application Support/dev.prowl.prowl"),
        }
    }

    #[test]
    fn launchd_agent_restarts_and_runs_at_login() {
        let plist = launchd_plist(&spec());
        assert!(plist.contains("<string>dev.prowl.prowld</string>"));
        assert!(plist.contains("<key>KeepAlive</key>\n    <true/>"));
        assert!(plist.contains("<key>RunAtLoad</key>\n    <true/>"));
        assert!(plist.contains("<string>/opt/Pr Owl/prowld-service</string>"));
        assert!(plist.contains("<string>--home</string>"));
        assert!(
            plist.contains("/Users/a&amp;b/Library"),
            "paths are XML-escaped"
        );
    }

    #[test]
    fn systemd_unit_restarts_always_and_quotes_paths() {
        let unit = systemd_unit(&spec());
        assert!(unit.contains("Restart=always"));
        assert!(unit.contains("WantedBy=default.target"));
        assert!(unit.contains(
            r#"ExecStart="/opt/Pr Owl/prowld-service" --home "/Users/a&b/Library/Application Support/dev.prowl.prowl""#
        ));
        assert_eq!(systemd_quote(r#"a"b\c%d$e"#), r#""a\"b\\c%%d$$e""#);
    }

    #[test]
    fn task_runs_at_logon_hidden_without_time_limit() {
        let xml = task_scheduler_xml(&spec(), r"WORK\amir");
        assert!(xml.contains(r"<UserId>WORK\amir</UserId>"));
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(xml.contains("<Hidden>true</Hidden>"));
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(xml.contains("<RestartOnFailure>"));
        assert!(xml.contains("<Command>/opt/Pr Owl/prowld-service</Command>"));
        assert!(xml.contains(r#"<Arguments>--home "/Users/a&amp;b/"#));
    }
}
