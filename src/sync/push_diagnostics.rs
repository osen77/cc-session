//! Content-free, operation-scoped push diagnostics. No authorization is cached here.
use std::cell::Cell;
use std::time::Instant;

#[derive(Clone, Copy, Default)]
struct Counters {
    enumerate: u64,
    probe: u64,
    diskutil: u64,
    candidates: u64,
    parsed: u64,
    snapshot_bytes: u64,
    snapshot_us: u64,
}

thread_local! {
    static COUNTERS: Cell<Option<Counters>> = const { Cell::new(None) };
}

pub(crate) struct Operation;
impl Operation {
    pub(crate) fn start(enabled: bool) -> Self {
        COUNTERS.with(|slot| slot.set(enabled.then(Counters::default)));
        Self
    }
}
impl Drop for Operation {
    fn drop(&mut self) {
        COUNTERS.with(|slot| slot.set(None));
    }
}

fn count(update: impl FnOnce(&mut Counters)) {
    COUNTERS.with(|slot| {
        if let Some(mut counters) = slot.get() {
            update(&mut counters);
            slot.set(Some(counters));
        }
    });
}
pub(crate) fn enumerate() { count(|c| c.enumerate += 1); }
pub(crate) fn probe() { count(|c| c.probe += 1); }
pub(crate) fn diskutil() { count(|c| c.diskutil += 1); }
pub(crate) fn candidate() { count(|c| c.candidates += 1); }
pub(crate) fn parsed() { count(|c| c.parsed += 1); }
pub(crate) fn snapshot(bytes: u64, elapsed: std::time::Duration) {
    count(|c| {
        c.snapshot_bytes += bytes;
        c.snapshot_us += elapsed.as_micros().min(u64::MAX as u128) as u64;
    });
}

pub(crate) struct Stage {
    name: &'static str,
    started: Instant,
}
impl Stage {
    pub(crate) fn start(name: &'static str) -> Self {
        if COUNTERS.with(|slot| slot.get().is_some()) {
            eprintln!("CCS_PUSH_PERF event=start stage={name}");
        }
        Self { name, started: Instant::now() }
    }
}
impl Drop for Stage {
    fn drop(&mut self) {
        COUNTERS.with(|slot| {
            if let Some(c) = slot.get() {
                eprintln!("CCS_PUSH_PERF event=end stage={} elapsed_ms={} enumerate={} probe={} diskutil={} candidates={} parsed={} snapshot_bytes={} snapshot_us={}", self.name, self.started.elapsed().as_millis(), c.enumerate, c.probe, c.diskutil, c.candidates, c.parsed, c.snapshot_bytes, c.snapshot_us);
            }
        });
    }
}
