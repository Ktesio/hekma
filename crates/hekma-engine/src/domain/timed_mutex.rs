//! The engine's lock instrumentation (story 24-4, AD-18 Step 1): a mutex
//! that, when timing is switched on, records how long each call site held
//! it — the per-site max and p99 the lock-model steps are judged by.
//!
//! [`TimedMutex::lock`] has `Mutex::lock`'s shape (a `LockResult` of a
//! guard that derefs to the value), so call sites do not change. The call
//! site is captured with `#[track_caller]`. With timing off (the default),
//! a lock costs one relaxed atomic load beyond the mutex itself.

use std::collections::{HashMap, VecDeque};
use std::panic::Location;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LockResult, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// How many recent holds each site keeps for its p99.
const SITE_SAMPLES: usize = 1024;

/// One call site's hold times.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LockHoldStats {
    /// The lock (`registry` or `supervisor`).
    pub lock: String,
    /// Where the guard was taken (`file:line`).
    pub site: String,
    /// Holds recorded since timing was switched on.
    pub holds: u64,
    /// The longest hold, in milliseconds.
    pub max_ms: f64,
    /// The p99 over the most recent holds, in milliseconds.
    pub p99_ms: f64,
}

#[derive(Debug, Default)]
struct Site {
    holds: u64,
    max: Duration,
    recent: VecDeque<Duration>,
}

/// The shared switch and the per-site records of every [`TimedMutex`] an
/// engine owns.
#[derive(Debug, Default)]
pub(crate) struct LockTiming {
    enabled: AtomicBool,
    sites: Mutex<HashMap<(&'static str, &'static Location<'static>), Site>>,
}

impl LockTiming {
    /// Switch recording on or off; switching on clears earlier records.
    pub(crate) fn set_enabled(&self, on: bool) {
        if on {
            self.sites
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clear();
        }
        self.enabled.store(on, Ordering::Relaxed);
    }

    fn record(&self, lock: &'static str, site: &'static Location<'static>, held: Duration) {
        let mut sites = self.sites.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = sites.entry((lock, site)).or_default();
        entry.holds += 1;
        entry.max = entry.max.max(held);
        if entry.recent.len() == SITE_SAMPLES {
            entry.recent.pop_front();
        }
        entry.recent.push_back(held);
    }

    /// Every recorded site, the longest max first.
    pub(crate) fn snapshot(&self) -> Vec<LockHoldStats> {
        let sites = self.sites.lock().unwrap_or_else(PoisonError::into_inner);
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        let mut out: Vec<LockHoldStats> = sites
            .iter()
            .map(|((lock, site), entry)| {
                let mut recent: Vec<Duration> = entry.recent.iter().copied().collect();
                recent.sort();
                let index = ((recent.len() as f64) * 0.99).ceil() as usize;
                let p99 = recent
                    .get(index.saturating_sub(1).min(recent.len().saturating_sub(1)))
                    .copied()
                    .unwrap_or_default();
                LockHoldStats {
                    lock: (*lock).to_string(),
                    site: format!("{}:{}", site.file(), site.line()),
                    holds: entry.holds,
                    max_ms: ms(entry.max),
                    p99_ms: ms(p99),
                }
            })
            .collect();
        out.sort_by(|a, b| b.max_ms.total_cmp(&a.max_ms));
        out
    }
}

/// A mutex whose holds are timed per call site when its [`LockTiming`] is
/// on.
#[derive(Debug)]
pub(crate) struct TimedMutex<T> {
    name: &'static str,
    timing: Arc<LockTiming>,
    inner: Mutex<T>,
}

impl<T> TimedMutex<T> {
    /// A mutex named `name` (the stats' `lock`) recording into `timing`.
    pub(crate) fn new(name: &'static str, timing: Arc<LockTiming>, value: T) -> Self {
        TimedMutex {
            name,
            timing,
            inner: Mutex::new(value),
        }
    }

    /// Lock it, as `Mutex::lock` does; the guard records its hold on drop
    /// when timing is on.
    #[track_caller]
    pub(crate) fn lock(&self) -> LockResult<TimedGuard<'_, T>> {
        let site = Location::caller();
        let timed = self.timing.enabled.load(Ordering::Relaxed);
        let (guard, poisoned) = match self.inner.lock() {
            Ok(guard) => (guard, false),
            Err(poisoned) => (poisoned.into_inner(), true),
        };
        let guard = self.guard(guard, timed, site);
        if poisoned {
            Err(PoisonError::new(guard))
        } else {
            Ok(guard)
        }
    }

    fn guard<'a>(
        &'a self,
        guard: MutexGuard<'a, T>,
        timed: bool,
        site: &'static Location<'static>,
    ) -> TimedGuard<'a, T> {
        TimedGuard {
            guard,
            started: timed.then(Instant::now),
            name: self.name,
            site,
            timing: &self.timing,
        }
    }
}

/// A [`TimedMutex`] guard.
pub(crate) struct TimedGuard<'a, T> {
    guard: MutexGuard<'a, T>,
    started: Option<Instant>,
    name: &'static str,
    site: &'static Location<'static>,
    timing: &'a LockTiming,
}

impl<T> std::ops::Deref for TimedGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> std::ops::DerefMut for TimedGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

impl<T> Drop for TimedGuard<'_, T> {
    fn drop(&mut self) {
        if let Some(started) = self.started {
            self.timing.record(self.name, self.site, started.elapsed());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_are_recorded_per_site_only_while_timing_is_on() {
        let timing = Arc::new(LockTiming::default());
        let lock = TimedMutex::new("registry", Arc::clone(&timing), 0u32);
        *lock.lock().unwrap() += 1;
        assert!(timing.snapshot().is_empty(), "off by default");
        timing.set_enabled(true);
        {
            let mut held = lock.lock().unwrap();
            *held += 1;
            std::thread::sleep(Duration::from_millis(30));
        }
        for _ in 0..3 {
            let _quick = lock.lock().unwrap();
        }
        let stats = timing.snapshot();
        assert_eq!(stats.len(), 2, "two call sites: {stats:?}");
        assert_eq!(stats[0].lock, "registry");
        assert!(stats[0].max_ms >= 30.0, "{stats:?}");
        assert_eq!(stats[0].holds, 1);
        assert!(stats[0].site.contains("timed_mutex.rs"), "{stats:?}");
        assert_eq!(stats[1].holds, 3);
        assert!(stats[1].p99_ms <= stats[0].max_ms);
        assert_eq!(*lock.lock().unwrap(), 2);
        // Switching on again clears; switching off stops recording.
        timing.set_enabled(true);
        assert!(timing.snapshot().is_empty());
        timing.set_enabled(false);
        let _off = lock.lock().unwrap();
        drop(_off);
        assert!(timing.snapshot().is_empty());
    }

    #[test]
    fn a_poisoned_lock_still_hands_out_its_value() {
        let timing = Arc::new(LockTiming::default());
        let lock = Arc::new(TimedMutex::new("supervisor", timing, 7u32));
        let poisoner = Arc::clone(&lock);
        let _ = std::thread::spawn(move || {
            let _held = poisoner.lock().unwrap();
            panic!("poison it");
        })
        .join();
        let value = *lock.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(value, 7);
    }
}
