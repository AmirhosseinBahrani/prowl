use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};

use prowld::PowerBackend;

/// Stand-in for the OS power APIs: tests set the power source and lid, simulate sleep by
/// moving the wall clock forward, and check whether PRowl is keeping the machine awake.
#[derive(Default)]
pub struct FakePower {
    on_ac: Mutex<Option<bool>>,
    battery: AtomicBool,
    lid_closed: Mutex<Option<bool>>,
    held: AtomicBool,
    holds: AtomicUsize,
    wall_offset_ms: AtomicI64,
}

impl FakePower {
    /// A laptop on mains power.
    pub fn laptop() -> Self {
        let fake = Self::default();
        fake.battery.store(true, Ordering::SeqCst);
        *fake.on_ac.lock().unwrap() = Some(true);
        fake
    }

    /// A machine without a battery.
    pub fn desktop() -> Self {
        Self::default()
    }

    pub fn set_on_ac(&self, on_ac: bool) {
        *self.on_ac.lock().unwrap() = Some(on_ac);
    }

    pub fn set_lid_closed(&self, closed: bool) {
        *self.lid_closed.lock().unwrap() = Some(closed);
    }

    /// Makes the wall clock jump forward, as if the machine had slept this long.
    pub fn sleep_for(&self, duration: std::time::Duration) {
        self.wall_offset_ms
            .fetch_add(duration.as_millis() as i64, Ordering::SeqCst);
    }

    pub fn is_held(&self) -> bool {
        self.held.load(Ordering::SeqCst)
    }

    /// How many times the sleep assertion was taken.
    pub fn holds(&self) -> usize {
        self.holds.load(Ordering::SeqCst)
    }
}

impl PowerBackend for FakePower {
    fn on_ac_power(&self) -> Option<bool> {
        *self.on_ac.lock().unwrap()
    }

    fn has_battery(&self) -> bool {
        self.battery.load(Ordering::SeqCst)
    }

    fn hold(&self) -> anyhow::Result<()> {
        if !self.held.swap(true, Ordering::SeqCst) {
            self.holds.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }

    fn release(&self) {
        self.held.store(false, Ordering::SeqCst);
    }

    fn lid_closed(&self) -> Option<bool> {
        *self.lid_closed.lock().unwrap()
    }

    fn wall_ms(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or_default()
            + self.wall_offset_ms.load(Ordering::SeqCst)
    }
}
