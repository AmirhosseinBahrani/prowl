//! Keeping the machine awake so reviews keep running, and catching up after it slept.
//!
//! Sleep is detected portably: a periodic tick compares wall-clock time with the time that
//! should have passed. A jump well beyond the tick interval means the machine was asleep (or
//! the process suspended), so PRowl polls straight away and reports the gap.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::events::{self, EventBus};
use crate::poller::Poller;

pub mod kind {
    pub const HELD: &str = "power.awake_held";
    pub const RELEASED: &str = "power.awake_released";
    pub const WOKE: &str = "power.woke";
    pub const LID_CLOSED: &str = "power.lid_closed";
    pub const UNAVAILABLE: &str = "power.unavailable";
}

/// When PRowl keeps the machine from idle-sleeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SleepMode {
    Always,
    /// Only while on mains power (the default on machines with a battery).
    AcOnly,
    /// Only while a review is running.
    DuringReviews,
}

/// The OS side: power source, sleep prevention, wall clock. Swappable for tests.
pub trait PowerBackend: Send + Sync + 'static {
    /// `Some(true)` on mains power, `Some(false)` on battery, `None` if unknown.
    fn on_ac_power(&self) -> Option<bool>;
    fn has_battery(&self) -> bool;
    /// Starts preventing idle system sleep (the display may still sleep).
    fn hold(&self) -> anyhow::Result<()>;
    fn release(&self);
    /// `Some(true)` when a laptop lid is closed, if the OS can tell.
    fn lid_closed(&self) -> Option<bool> {
        None
    }
    /// Wall-clock time in Unix milliseconds.
    fn wall_ms(&self) -> i64 {
        events::now_ms()
    }
}

/// A [`PowerBackend`] handed to [`Config`](crate::Config), e.g. a fake in tests.
#[derive(Clone)]
pub struct PowerHook(pub Arc<dyn PowerBackend>);

impl std::fmt::Debug for PowerHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PowerHook(..)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerStatus {
    pub mode: SleepMode,
    /// Whether PRowl is currently keeping the machine awake.
    pub holding: bool,
    pub on_ac_power: Option<bool>,
    pub has_battery: bool,
}

#[derive(Clone)]
pub(crate) struct Power {
    inner: Arc<Inner>,
}

struct Inner {
    backend: Arc<dyn PowerBackend>,
    mode: SleepMode,
    events: EventBus,
    /// Reviews currently running (maintained by the worker).
    active: Arc<AtomicUsize>,
    holding: std::sync::Mutex<bool>,
}

pub(crate) struct PowerSettings {
    pub check_every: Duration,
    /// How far wall-clock time may run ahead of a tick before it counts as sleep.
    pub wake_gap: Duration,
}

impl Power {
    pub(crate) fn new(
        backend: Arc<dyn PowerBackend>,
        mode: Option<SleepMode>,
        events: EventBus,
        active: Arc<AtomicUsize>,
    ) -> Self {
        let mode = mode.unwrap_or(if backend.has_battery() {
            SleepMode::AcOnly
        } else {
            SleepMode::Always
        });
        Self {
            inner: Arc::new(Inner {
                backend,
                mode,
                events,
                active,
                holding: std::sync::Mutex::new(false),
            }),
        }
    }

    pub(crate) fn status(&self) -> PowerStatus {
        PowerStatus {
            mode: self.inner.mode,
            holding: *self.inner.holding.lock().unwrap(),
            on_ac_power: self.inner.backend.on_ac_power(),
            has_battery: self.inner.backend.has_battery(),
        }
    }

    pub(crate) fn spawn(
        &self,
        settings: PowerSettings,
        poller: Poller,
        shutdown: CancellationToken,
    ) {
        let this = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(settings.check_every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let expected = settings.check_every.as_millis() as i64;
            let gap = settings.wake_gap.as_millis() as i64;
            let mut last_wall = this.inner.backend.wall_ms();
            let mut lid = None;
            let mut unavailable_reported = false;
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    () = shutdown.cancelled() => break,
                }
                let now = this.inner.backend.wall_ms();
                if now - last_wall > expected + gap {
                    this.woke(last_wall, now, &poller).await;
                }
                last_wall = now;

                let closed = this.inner.backend.lid_closed();
                if closed == Some(true) && lid != Some(true) {
                    let _ = this
                        .inner
                        .events
                        .emit(
                            kind::LID_CLOSED,
                            serde_json::json!({
                                "message": "The lid is closed. The Mac keeps reviewing only in clamshell mode \
                                            (external display and power); otherwise it sleeps and reviews pause."
                            }),
                        )
                        .await;
                }
                lid = closed;

                if let Err(e) = this.apply().await
                    && !unavailable_reported
                {
                    unavailable_reported = true;
                    tracing::warn!(error = %format!("{e:#}"), "cannot keep the machine awake");
                    let _ = this
                        .inner
                        .events
                        .emit(
                            kind::UNAVAILABLE,
                            serde_json::json!({ "error": format!("{e:#}") }),
                        )
                        .await;
                }
            }
            this.set_holding(false, "shutdown").await;
        });
    }

    /// Holds or releases the sleep assertion to match the mode and current state.
    async fn apply(&self) -> anyhow::Result<()> {
        let (want, reason) = match self.inner.mode {
            SleepMode::Always => (true, "always"),
            SleepMode::AcOnly => match self.inner.backend.on_ac_power() {
                Some(false) => (false, "on_battery"),
                _ => (true, "on_ac_power"),
            },
            SleepMode::DuringReviews => {
                if self.inner.active.load(Ordering::SeqCst) > 0 {
                    (true, "review_running")
                } else {
                    (false, "idle")
                }
            }
        };
        let holding = *self.inner.holding.lock().unwrap();
        if want && !holding {
            self.inner.backend.hold()?;
        }
        self.set_holding(want, reason).await;
        Ok(())
    }

    async fn set_holding(&self, want: bool, reason: &str) {
        let changed = {
            let mut holding = self.inner.holding.lock().unwrap();
            let changed = *holding != want;
            if changed && !want {
                self.inner.backend.release();
            }
            *holding = want;
            changed
        };
        if changed {
            let _ = self
                .inner
                .events
                .emit(
                    if want { kind::HELD } else { kind::RELEASED },
                    serde_json::json!({ "mode": self.inner.mode, "reason": reason }),
                )
                .await;
        }
    }

    async fn woke(&self, asleep_from_ms: i64, asleep_to_ms: i64, poller: &Poller) {
        tracing::info!(
            asleep_from_ms,
            asleep_to_ms,
            "the machine was asleep; catching up"
        );
        let summary = poller.poll_once().await;
        let _ = self
            .inner
            .events
            .emit(
                kind::WOKE,
                serde_json::json!({
                    "asleep_from_ms": asleep_from_ms,
                    "asleep_to_ms": asleep_to_ms,
                    "caught_up": summary.enqueued,
                    "lid_closed": self.inner.backend.lid_closed(),
                }),
            )
            .await;
    }
}

/// The backend for the OS PRowl runs on.
pub(crate) fn platform_backend() -> Arc<dyn PowerBackend> {
    Arc::new(imp::Native::default())
}

#[cfg(target_os = "macos")]
mod imp {
    //! IOKit power assertions and power-source info; `ioreg` for the lid.
    use std::ffi::{CStr, c_char, c_void};
    use std::sync::Mutex;

    use super::PowerBackend;

    type CFTypeRef = *const c_void;
    type CFStringRef = *const c_void;
    const UTF8: u32 = 0x0800_0100;
    const ASSERTION_LEVEL_ON: u32 = 255;

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionCreateWithName(
            kind: CFStringRef,
            level: u32,
            name: CFStringRef,
            id: *mut u32,
        ) -> i32;
        fn IOPMAssertionRelease(id: u32) -> i32;
        fn IOPSCopyPowerSourcesInfo() -> CFTypeRef;
        fn IOPSCopyPowerSourcesList(blob: CFTypeRef) -> CFTypeRef;
        fn IOPSGetProvidingPowerSourceType(blob: CFTypeRef) -> CFStringRef;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCString(
            alloc: CFTypeRef,
            s: *const c_char,
            encoding: u32,
        ) -> CFStringRef;
        fn CFStringGetCString(s: CFStringRef, buf: *mut c_char, size: isize, encoding: u32) -> u8;
        fn CFArrayGetCount(array: CFTypeRef) -> isize;
        fn CFRelease(cf: CFTypeRef);
    }

    /// An owned CoreFoundation object, released on drop.
    struct Owned(CFTypeRef);

    impl Drop for Owned {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: we own this reference (it came from a Create/Copy call).
                unsafe { CFRelease(self.0) }
            }
        }
    }

    fn cf_string(s: &CStr) -> Owned {
        // SAFETY: `s` is a valid NUL-terminated string; a null allocator means the default.
        Owned(unsafe { CFStringCreateWithCString(std::ptr::null(), s.as_ptr(), UTF8) })
    }

    #[derive(Default)]
    pub struct Native {
        assertion: Mutex<Option<u32>>,
    }

    impl PowerBackend for Native {
        fn on_ac_power(&self) -> Option<bool> {
            // SAFETY: Copy functions return owned objects (or null); Get returns a borrowed
            // string valid while `info` lives; the buffer is large enough and NUL-terminated.
            unsafe {
                let info = Owned(IOPSCopyPowerSourcesInfo());
                if info.0.is_null() {
                    return None;
                }
                let source = IOPSGetProvidingPowerSourceType(info.0);
                if source.is_null() {
                    return None;
                }
                let mut buf = [0 as c_char; 64];
                if CFStringGetCString(source, buf.as_mut_ptr(), buf.len() as isize, UTF8) == 0 {
                    return None;
                }
                let name = CStr::from_ptr(buf.as_ptr()).to_string_lossy();
                Some(name == "AC Power")
            }
        }

        fn has_battery(&self) -> bool {
            // SAFETY: as above; the list is owned and released.
            unsafe {
                let info = Owned(IOPSCopyPowerSourcesInfo());
                if info.0.is_null() {
                    return false;
                }
                let list = Owned(IOPSCopyPowerSourcesList(info.0));
                !list.0.is_null() && CFArrayGetCount(list.0) > 0
            }
        }

        fn hold(&self) -> anyhow::Result<()> {
            let mut assertion = self.assertion.lock().unwrap();
            if assertion.is_some() {
                return Ok(());
            }
            let kind = cf_string(c"PreventUserIdleSystemSleep");
            let name = cf_string(c"PRowl is reviewing pull requests");
            let mut id = 0u32;
            // SAFETY: both strings are valid CFStrings for the duration of the call.
            let status =
                unsafe { IOPMAssertionCreateWithName(kind.0, ASSERTION_LEVEL_ON, name.0, &mut id) };
            anyhow::ensure!(status == 0, "IOPMAssertionCreateWithName failed ({status})");
            *assertion = Some(id);
            Ok(())
        }

        fn release(&self) {
            if let Some(id) = self.assertion.lock().unwrap().take() {
                // SAFETY: `id` is an assertion we created and haven't released.
                unsafe { IOPMAssertionRelease(id) };
            }
        }

        fn lid_closed(&self) -> Option<bool> {
            let out = std::process::Command::new("ioreg")
                .args(["-r", "-k", "AppleClamshellState", "-d", "4"])
                .output()
                .ok()?;
            let text = String::from_utf8_lossy(&out.stdout);
            let line = text
                .lines()
                .find(|l| l.contains("\"AppleClamshellState\""))?;
            Some(line.trim_end().ends_with("Yes"))
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod imp {
    //! systemd-inhibit for sleep prevention; /sys/class/power_supply for the power source.
    use std::path::Path;
    use std::process::{Child, Command, Stdio};
    use std::sync::Mutex;

    use anyhow::Context;

    use super::PowerBackend;

    #[derive(Default)]
    pub struct Native {
        inhibitor: Mutex<Option<Child>>,
    }

    fn supplies() -> Vec<(String, std::path::PathBuf)> {
        std::fs::read_dir("/sys/class/power_supply")
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let kind = std::fs::read_to_string(e.path().join("type")).ok()?;
                Some((kind.trim().to_owned(), e.path()))
            })
            .collect()
    }

    fn read(path: &Path, file: &str) -> Option<String> {
        std::fs::read_to_string(path.join(file))
            .ok()
            .map(|s| s.trim().to_owned())
    }

    impl PowerBackend for Native {
        fn on_ac_power(&self) -> Option<bool> {
            let all = supplies();
            if let Some(online) = all
                .iter()
                .filter(|(kind, _)| kind == "Mains" || kind == "USB")
                .filter_map(|(_, path)| read(path, "online"))
                .find(|online| online == "1")
            {
                return Some(online == "1");
            }
            all.iter()
                .any(|(kind, _)| kind == "Battery")
                .then_some(false)
        }

        fn has_battery(&self) -> bool {
            supplies().iter().any(|(kind, _)| kind == "Battery")
        }

        fn hold(&self) -> anyhow::Result<()> {
            let mut inhibitor = self.inhibitor.lock().unwrap();
            if let Some(child) = inhibitor.as_mut()
                && child.try_wait().ok().flatten().is_none()
            {
                return Ok(());
            }
            let child = Command::new("systemd-inhibit")
                .args([
                    "--what=idle:sleep",
                    "--who=PRowl",
                    "--why=Reviewing pull requests",
                    "--mode=block",
                    "sleep",
                    "infinity",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .context("starting systemd-inhibit (is systemd available?)")?;
            *inhibitor = Some(child);
            Ok(())
        }

        fn release(&self) {
            if let Some(mut child) = self.inhibitor.lock().unwrap().take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

#[cfg(windows)]
mod imp {
    //! SetThreadExecutionState on a dedicated thread; GetSystemPowerStatus for the source.
    use std::sync::Mutex;
    use std::sync::mpsc::{Sender, channel};

    use windows_sys::Win32::System::Power::{
        ES_CONTINUOUS, ES_SYSTEM_REQUIRED, GetSystemPowerStatus, SYSTEM_POWER_STATUS,
        SetThreadExecutionState,
    };

    use super::PowerBackend;

    #[derive(Default)]
    pub struct Native {
        /// Dropping or signalling it makes the holding thread clear its state and exit.
        stop: Mutex<Option<Sender<()>>>,
    }

    fn status() -> Option<SYSTEM_POWER_STATUS> {
        // SAFETY: the struct is plain data and fully written on success.
        unsafe {
            let mut status: SYSTEM_POWER_STATUS = std::mem::zeroed();
            (GetSystemPowerStatus(&mut status) != 0).then_some(status)
        }
    }

    impl PowerBackend for Native {
        fn on_ac_power(&self) -> Option<bool> {
            match status()?.ACLineStatus {
                0 => Some(false),
                1 => Some(true),
                _ => None,
            }
        }

        fn has_battery(&self) -> bool {
            // 128: no system battery; 255: unknown.
            status().is_some_and(|s| s.BatteryFlag != 128 && s.BatteryFlag != 255)
        }

        fn hold(&self) -> anyhow::Result<()> {
            let mut stop = self.stop.lock().unwrap();
            if stop.is_some() {
                return Ok(());
            }
            let (tx, rx) = channel::<()>();
            // Execution state belongs to the calling thread, so one thread owns it.
            std::thread::Builder::new()
                .name("prowl-keep-awake".into())
                .spawn(move || {
                    // SAFETY: plain Win32 call with valid flags.
                    unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) };
                    let _ = rx.recv();
                    // SAFETY: as above; clears the requirement.
                    unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
                })?;
            *stop = Some(tx);
            Ok(())
        }

        fn release(&self) {
            if let Some(tx) = self.stop.lock().unwrap().take() {
                let _ = tx.send(());
            }
        }
    }
}
