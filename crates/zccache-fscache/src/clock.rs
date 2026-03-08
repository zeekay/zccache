//! Monotonic clock and change journal for batched file-change tracking.
//!
//! The daemon's watcher produces settled batches of changed paths. Each batch
//! advances the clock by one tick. Clients remember their last-seen clock and
//! ask "has this file changed since clock N?" — an O(1) lookup that eliminates
//! redundant stat calls across concurrent compilations.

use dashmap::DashMap;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

/// Monotonically increasing clock tick.
///
/// Each settled batch of file-change events increments the clock by one.
/// Clients hold onto a `Clock` value and use it to ask the journal
/// "has anything changed since I last checked?"
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Clock(u64);

impl Clock {
    /// The epoch — before any changes have been recorded.
    pub const ZERO: Clock = Clock(0);

    /// Returns the raw tick value.
    #[must_use]
    pub fn tick(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for Clock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "clock:{}", self.0)
    }
}

/// Tracks which files changed at which clock tick.
///
/// Two complementary structures:
/// - `journal`: `BTreeMap<Clock, Vec<PathBuf>>` — ordered by clock, for
///   "give me everything changed since clock N" queries. Bounded.
/// - `last_change`: `DashMap<PathBuf, Clock>` — per-file last-change clock,
///   for O(1) "has this specific file changed since clock N?" checks.
pub struct ChangeJournal {
    current: AtomicU64,
    journal: RwLock<BTreeMap<Clock, Vec<PathBuf>>>,
    last_change: DashMap<PathBuf, Clock>,
    /// Clock at which the last overflow occurred. Any query with
    /// `since < last_overflow` returns "changed" for all files.
    last_overflow: AtomicU64,
    max_journal_entries: usize,
}

impl std::fmt::Debug for ChangeJournal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChangeJournal")
            .field("current", &self.current_clock())
            .field("entries", &self.last_change.len())
            .finish()
    }
}

impl ChangeJournal {
    /// Create a new journal with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(10_000)
    }

    /// Create a new journal with a custom max journal size.
    #[must_use]
    pub fn with_capacity(max_journal_entries: usize) -> Self {
        Self {
            current: AtomicU64::new(0),
            journal: RwLock::new(BTreeMap::new()),
            last_change: DashMap::new(),
            last_overflow: AtomicU64::new(0),
            max_journal_entries,
        }
    }

    /// Returns the current clock value.
    #[must_use]
    pub fn current_clock(&self) -> Clock {
        Clock(self.current.load(Ordering::Acquire))
    }

    /// Record a batch of changed paths, advancing the clock by one.
    ///
    /// Returns the new clock value.
    pub fn advance(&self, changed_paths: Vec<PathBuf>) -> Clock {
        let new_tick = self.current.fetch_add(1, Ordering::AcqRel) + 1;
        let clock = Clock(new_tick);

        // Update per-file last-change clock.
        for path in &changed_paths {
            self.last_change.insert(path.clone(), clock);
        }

        // Append to the ordered journal.
        let mut journal = self.journal.write().expect("journal lock poisoned");
        journal.insert(clock, changed_paths);

        // Trim old entries if over capacity.
        while journal.len() > self.max_journal_entries {
            journal.pop_first();
        }

        clock
    }

    /// Check if a specific file has changed since `since_clock`.
    ///
    /// Returns `true` if:
    /// - The file was modified after `since_clock`, OR
    /// - An overflow occurred after `since_clock`, OR
    /// - The file has never been seen by the journal (conservative).
    ///
    /// Returns `false` only when we have positive evidence that the file
    /// has NOT changed since the given clock.
    #[must_use]
    pub fn changed_since(&self, path: &Path, since: Clock) -> bool {
        // Overflow invalidates everything before it.
        let overflow = self.last_overflow.load(Ordering::Acquire);
        if overflow > 0 && since.0 < overflow {
            return true;
        }

        // If the file is tracked, check its last-change clock.
        match self.last_change.get(path) {
            Some(last) => *last > since,
            // File never seen by journal — conservative: assume changed.
            None => true,
        }
    }

    /// Return all paths changed since `since_clock`.
    ///
    /// Uses the BTreeMap range query. May return incomplete results
    /// for very old clocks if journal entries have been trimmed.
    #[must_use]
    pub fn changes_since(&self, since: Clock) -> Vec<PathBuf> {
        let journal = self.journal.read().expect("journal lock poisoned");
        let mut result = Vec::new();
        for (_clock, paths) in
            journal.range((std::ops::Bound::Excluded(since), std::ops::Bound::Unbounded))
        {
            result.extend(paths.iter().cloned());
        }
        result
    }

    /// Record an overflow event.
    ///
    /// Advances the clock and marks the overflow point. Any `changed_since`
    /// query with `since < overflow_clock` will return `true` for all files.
    pub fn mark_overflow(&self) -> Clock {
        let new_tick = self.current.fetch_add(1, Ordering::AcqRel) + 1;
        self.last_overflow.store(new_tick, Ordering::Release);
        Clock(new_tick)
    }
}

impl Default for ChangeJournal {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_display() {
        let c = Clock(42);
        assert_eq!(format!("{c}"), "clock:42");
        assert_eq!(format!("{}", Clock::ZERO), "clock:0");
    }

    #[test]
    fn clock_zero_tick() {
        assert_eq!(Clock::ZERO.tick(), 0);
    }

    #[test]
    fn clock_ordering() {
        assert!(Clock::ZERO < Clock(1));
        assert!(Clock(1) < Clock(2));
        assert_eq!(Clock(5), Clock(5));
    }

    #[test]
    fn advance_empty_batch() {
        let journal = ChangeJournal::new();
        let c1 = journal.advance(vec![]);
        assert_eq!(c1, Clock(1));
        assert_eq!(journal.current_clock(), Clock(1));

        // Empty batch still advances clock but has no paths.
        let changes = journal.changes_since(Clock::ZERO);
        assert!(changes.is_empty());
    }

    #[test]
    fn changes_since_empty_journal() {
        let journal = ChangeJournal::new();
        let changes = journal.changes_since(Clock::ZERO);
        assert!(changes.is_empty());
    }

    #[test]
    fn default_creates_new_journal() {
        let journal = ChangeJournal::default();
        assert_eq!(journal.current_clock(), Clock::ZERO);
    }

    #[test]
    fn changed_since_at_exact_clock_returns_false() {
        let journal = ChangeJournal::new();
        let c1 = journal.advance(vec![PathBuf::from("a.c")]);
        // File changed AT c1, asking since c1 → false (not AFTER c1).
        assert!(!journal.changed_since(Path::new("a.c"), c1));
    }

    #[test]
    fn overflow_then_new_changes() {
        let journal = ChangeJournal::new();
        let c1 = journal.advance(vec![PathBuf::from("a.c")]);
        let overflow_clock = journal.mark_overflow();

        // New change after overflow.
        let c3 = journal.advance(vec![PathBuf::from("b.c")]);

        // Before overflow → always changed.
        assert!(journal.changed_since(Path::new("a.c"), c1));
        // After overflow → normal behavior.
        assert!(!journal.changed_since(Path::new("b.c"), c3));
        assert!(journal.changed_since(Path::new("b.c"), overflow_clock));
    }

    #[test]
    fn advance_increments_clock() {
        let journal = ChangeJournal::new();
        assert_eq!(journal.current_clock(), Clock::ZERO);

        let c1 = journal.advance(vec![PathBuf::from("a.c")]);
        assert_eq!(c1, Clock(1));

        let c2 = journal.advance(vec![PathBuf::from("b.c")]);
        assert_eq!(c2, Clock(2));

        assert_eq!(journal.current_clock(), Clock(2));
    }

    #[test]
    fn changed_since_returns_true_for_changed_file() {
        let journal = ChangeJournal::new();

        let c1 = journal.advance(vec![PathBuf::from("foo.h")]);
        let c2 = journal.advance(vec![PathBuf::from("bar.h")]);

        // foo.h changed at c1 — asking since ZERO should be true.
        assert!(journal.changed_since(Path::new("foo.h"), Clock::ZERO));
        // bar.h changed at c2 — asking since c1 should be true.
        assert!(journal.changed_since(Path::new("bar.h"), c1));
        // bar.h changed at c2 — asking since c2 should be false (not AFTER c2).
        assert!(!journal.changed_since(Path::new("bar.h"), c2));
    }

    #[test]
    fn changed_since_returns_false_for_unchanged_file() {
        let journal = ChangeJournal::new();

        let c1 = journal.advance(vec![PathBuf::from("foo.h")]);
        // bar.h was never changed. But since it's untracked, conservative = true.
        // To get false, bar.h must have been in a batch.
        let _c2 = journal.advance(vec![PathBuf::from("bar.h")]);

        // foo.h last changed at c1. Asking since c1 → false (not after c1).
        assert!(!journal.changed_since(Path::new("foo.h"), c1));
    }

    #[test]
    fn untracked_file_reports_changed() {
        let journal = ChangeJournal::new();
        journal.advance(vec![PathBuf::from("known.h")]);

        // A file the journal has never seen → conservative true.
        assert!(journal.changed_since(Path::new("unknown.h"), Clock::ZERO));
    }

    #[test]
    fn changes_since_returns_batch_union() {
        let journal = ChangeJournal::new();

        let c1 = journal.advance(vec![PathBuf::from("a.c")]);
        let _c2 = journal.advance(vec![PathBuf::from("b.c"), PathBuf::from("c.c")]);
        let _c3 = journal.advance(vec![PathBuf::from("d.c")]);

        let changed = journal.changes_since(c1);
        assert_eq!(changed.len(), 3);
        assert!(changed.contains(&PathBuf::from("b.c")));
        assert!(changed.contains(&PathBuf::from("c.c")));
        assert!(changed.contains(&PathBuf::from("d.c")));
    }

    #[test]
    fn journal_trims_old_entries() {
        let journal = ChangeJournal::with_capacity(5);

        for i in 0..10 {
            journal.advance(vec![PathBuf::from(format!("file_{i}.c"))]);
        }

        let journal_entries = journal.journal.read().unwrap();
        assert!(journal_entries.len() <= 5);
        // Oldest entries should be gone.
        assert!(!journal_entries.contains_key(&Clock(1)));
        // Newest should remain.
        assert!(journal_entries.contains_key(&Clock(10)));
    }

    #[test]
    fn mark_overflow_invalidates_everything() {
        let journal = ChangeJournal::new();

        let c1 = journal.advance(vec![PathBuf::from("a.c")]);
        let _c2 = journal.advance(vec![PathBuf::from("b.c")]);
        let overflow_clock = journal.mark_overflow();

        // Queries with clock before overflow → always true.
        assert!(journal.changed_since(Path::new("a.c"), c1));
        assert!(journal.changed_since(Path::new("b.c"), c1));
        // Even untracked files.
        assert!(journal.changed_since(Path::new("never_seen.c"), c1));

        // Queries at or after overflow clock → normal behavior.
        // a.c last changed at c1, which is before overflow_clock,
        // but the query is since=overflow_clock which is >= overflow,
        // so overflow check doesn't trigger.
        assert!(!journal.changed_since(Path::new("a.c"), overflow_clock));
    }

    #[test]
    fn concurrent_advance_and_query() {
        use std::sync::Arc;

        let journal = Arc::new(ChangeJournal::new());
        let mut handles = Vec::new();

        // Writers: 4 threads each advancing 100 times.
        for t in 0..4 {
            let j = Arc::clone(&journal);
            handles.push(std::thread::spawn(move || {
                for i in 0..100 {
                    j.advance(vec![PathBuf::from(format!("t{t}_f{i}.c"))]);
                }
            }));
        }

        // Readers: 4 threads each querying 100 times.
        for _ in 0..4 {
            let j = Arc::clone(&journal);
            handles.push(std::thread::spawn(move || {
                for _ in 0..100 {
                    let clock = j.current_clock();
                    let _ = j.changed_since(Path::new("t0_f0.c"), clock);
                    let _ = j.changes_since(Clock::ZERO);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // 4 threads × 100 advances = 400 ticks.
        assert_eq!(journal.current_clock().tick(), 400);
    }
}
