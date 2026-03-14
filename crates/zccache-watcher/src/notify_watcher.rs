//! Concrete file watcher backed by the `notify` crate.
//!
//! Creates a `RecommendedWatcher` that converts OS filesystem events into
//! `WatchEvent`s, filters them through an `IgnoreFilter`, and sends them
//! over a `tokio::sync::mpsc` channel for consumption by the settle buffer.
//!
//! The notify callback runs on a dedicated OS thread. Using
//! `tokio::sync::mpsc` (not crossbeam) ensures safe crossing from the OS
//! thread into the async runtime.

use crate::ignore::IgnoreFilter;
use crate::WatchEvent;
use notify::{Event, EventKind, RecursiveMode, Watcher};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;

/// File watcher backed by the `notify` crate.
///
/// Wraps a `RecommendedWatcher` and exposes `watch`/`unwatch` methods.
/// Events are sent to the unbounded receiver returned by [`NotifyWatcher::new`].
pub struct NotifyWatcher {
    watcher: notify::RecommendedWatcher,
}

impl std::fmt::Debug for NotifyWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotifyWatcher").finish_non_exhaustive()
    }
}

impl NotifyWatcher {
    /// Create a new watcher with the given ignore filter.
    ///
    /// Returns the watcher and an unbounded receiver of `WatchEvent`s.
    /// The receiver should be fed into a [`SettleBuffer`](crate::settle::SettleBuffer).
    ///
    /// # Errors
    ///
    /// Returns an error if the OS file watcher cannot be initialized.
    pub fn new(
        ignore: Arc<IgnoreFilter>,
    ) -> zccache_core::Result<(Self, mpsc::UnboundedReceiver<WatchEvent>)> {
        let (tx, rx) = mpsc::unbounded_channel();

        let watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            match res {
                Ok(event) => {
                    for watch_event in convert_event(&ignore, &event) {
                        if tx.send(watch_event).is_err() {
                            // Receiver dropped — watcher is shutting down.
                            return;
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("watcher error: {e}");
                    let _ = tx.send(WatchEvent::Error(e.to_string()));
                }
            }
        })
        .map_err(|e| std::io::Error::other(e.to_string()))?;

        Ok((Self { watcher }, rx))
    }

    /// Start watching a single directory (non-recursive).
    ///
    /// Callers are responsible for enumerating subdirectories and watching
    /// each one individually. This avoids platform-level recursive watches
    /// that can hit OS limits or produce degenerate behaviour on large trees.
    ///
    /// # Errors
    ///
    /// Returns an error if the path cannot be watched.
    pub fn watch(&mut self, path: &Path) -> zccache_core::Result<()> {
        self.watcher
            .watch(path, RecursiveMode::NonRecursive)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(())
    }

    /// Stop watching a directory.
    ///
    /// # Errors
    ///
    /// Returns an error if the path was not being watched.
    pub fn unwatch(&mut self, path: &Path) -> zccache_core::Result<()> {
        self.watcher
            .unwatch(path)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(())
    }
}

/// Convert a `notify::Event` into zero or more `WatchEvent`s.
fn convert_event(ignore: &IgnoreFilter, event: &Event) -> Vec<WatchEvent> {
    // Detect overflow/rescan events from inotify (Q_OVERFLOW) and
    // FSEvents (MUST_SCAN_SUBDIRS). These have EventKind::Other with
    // Flag::Rescan and empty paths — the path loop below would produce
    // nothing, silently swallowing the overflow.
    if event.need_rescan() {
        return vec![WatchEvent::Overflow];
    }

    // Handle rename with both paths present.
    if matches!(
        event.kind,
        EventKind::Modify(notify::event::ModifyKind::Name(
            notify::event::RenameMode::Both
        ))
    ) && event.paths.len() >= 2
    {
        let from = &event.paths[0];
        let to = &event.paths[1];
        if ignore.should_ignore(from) && ignore.should_ignore(to) {
            return vec![];
        }
        return vec![WatchEvent::Renamed {
            from: from.clone(),
            to: to.clone(),
        }];
    }

    let mut result = Vec::new();
    for path in &event.paths {
        if ignore.should_ignore(path) {
            continue;
        }

        let watch_event = match event.kind {
            EventKind::Create(_) => WatchEvent::Created(path.clone()),
            EventKind::Remove(_) => WatchEvent::Removed(path.clone()),
            EventKind::Modify(notify::event::ModifyKind::Name(notify::event::RenameMode::From)) => {
                // Half of a rename — treat as removal (conservative).
                WatchEvent::Removed(path.clone())
            }
            EventKind::Modify(notify::event::ModifyKind::Name(notify::event::RenameMode::To)) => {
                // Half of a rename — treat as creation (conservative).
                WatchEvent::Created(path.clone())
            }
            EventKind::Modify(_) => WatchEvent::Modified(path.clone()),
            EventKind::Access(_) => continue,
            // Any, Other — conservative: treat as modification.
            _ => WatchEvent::Modified(path.clone()),
        };

        result.push(watch_event);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_filter() -> IgnoreFilter {
        IgnoreFilter::new(vec![".git".to_string(), "target".to_string()])
    }

    #[test]
    fn convert_create_event() {
        let filter = test_filter();
        let event = Event {
            kind: EventKind::Create(notify::event::CreateKind::File),
            paths: vec![PathBuf::from("src/main.rs")],
            attrs: Default::default(),
        };
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], WatchEvent::Created(p) if p == Path::new("src/main.rs")));
    }

    #[test]
    fn convert_modify_event() {
        let filter = test_filter();
        let event = Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content,
            )),
            paths: vec![PathBuf::from("src/lib.rs")],
            attrs: Default::default(),
        };
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], WatchEvent::Modified(p) if p == Path::new("src/lib.rs")));
    }

    #[test]
    fn convert_remove_event() {
        let filter = test_filter();
        let event = Event {
            kind: EventKind::Remove(notify::event::RemoveKind::File),
            paths: vec![PathBuf::from("old.c")],
            attrs: Default::default(),
        };
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], WatchEvent::Removed(p) if p == Path::new("old.c")));
    }

    #[test]
    fn convert_rename_both() {
        let filter = test_filter();
        let event = Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Name(
                notify::event::RenameMode::Both,
            )),
            paths: vec![PathBuf::from("old.c"), PathBuf::from("new.c")],
            attrs: Default::default(),
        };
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(matches!(
            &result[0],
            WatchEvent::Renamed { from, to }
            if from == Path::new("old.c") && to == Path::new("new.c")
        ));
    }

    #[test]
    fn convert_rename_from_becomes_removed() {
        let filter = test_filter();
        let event = Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Name(
                notify::event::RenameMode::From,
            )),
            paths: vec![PathBuf::from("gone.c")],
            attrs: Default::default(),
        };
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], WatchEvent::Removed(p) if p == Path::new("gone.c")));
    }

    #[test]
    fn convert_rename_to_becomes_created() {
        let filter = test_filter();
        let event = Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Name(
                notify::event::RenameMode::To,
            )),
            paths: vec![PathBuf::from("appeared.c")],
            attrs: Default::default(),
        };
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], WatchEvent::Created(p) if p == Path::new("appeared.c")));
    }

    #[test]
    fn ignored_paths_filtered_out() {
        let filter = test_filter();
        let event = Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content,
            )),
            paths: vec![PathBuf::from("project/.git/index")],
            attrs: Default::default(),
        };
        let result = convert_event(&filter, &event);
        assert!(result.is_empty());
    }

    #[test]
    fn ignored_rename_both_filtered() {
        let filter = test_filter();
        let event = Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Name(
                notify::event::RenameMode::Both,
            )),
            paths: vec![
                PathBuf::from("project/.git/old"),
                PathBuf::from("project/.git/new"),
            ],
            attrs: Default::default(),
        };
        let result = convert_event(&filter, &event);
        assert!(result.is_empty());
    }

    #[test]
    fn access_events_ignored() {
        let filter = test_filter();
        let event = Event {
            kind: EventKind::Access(notify::event::AccessKind::Read),
            paths: vec![PathBuf::from("src/main.rs")],
            attrs: Default::default(),
        };
        let result = convert_event(&filter, &event);
        assert!(result.is_empty());
    }

    #[test]
    fn rename_both_with_single_path_falls_through() {
        // Rename Both with < 2 paths should not panic; falls to per-path loop.
        let filter = test_filter();
        let event = Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Name(
                notify::event::RenameMode::Both,
            )),
            paths: vec![PathBuf::from("only_one.c")],
            attrs: Default::default(),
        };
        let result = convert_event(&filter, &event);
        // Falls through to per-path handling as Modify(Name(Both)), caught by wildcard → Modified.
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], WatchEvent::Modified(_)));
    }

    #[test]
    fn event_with_empty_paths() {
        let filter = test_filter();
        let event = Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content,
            )),
            paths: vec![],
            attrs: Default::default(),
        };
        let result = convert_event(&filter, &event);
        assert!(result.is_empty());
    }

    #[test]
    fn event_kind_other_becomes_modified() {
        let filter = test_filter();
        let event = Event {
            kind: EventKind::Other,
            paths: vec![PathBuf::from("mystery.c")],
            attrs: Default::default(),
        };
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], WatchEvent::Modified(_)));
    }

    #[test]
    fn event_kind_any_becomes_modified() {
        let filter = test_filter();
        let event = Event {
            kind: EventKind::Any,
            paths: vec![PathBuf::from("any.c")],
            attrs: Default::default(),
        };
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], WatchEvent::Modified(_)));
    }

    #[test]
    fn remove_directory_event() {
        let filter = test_filter();
        let event = Event {
            kind: EventKind::Remove(notify::event::RemoveKind::Folder),
            paths: vec![PathBuf::from("src/old_module")],
            attrs: Default::default(),
        };
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], WatchEvent::Removed(_)));
    }

    #[test]
    fn create_directory_event() {
        let filter = test_filter();
        let event = Event {
            kind: EventKind::Create(notify::event::CreateKind::Folder),
            paths: vec![PathBuf::from("src/new_module")],
            attrs: Default::default(),
        };
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], WatchEvent::Created(_)));
    }

    #[test]
    fn metadata_change_becomes_modified() {
        let filter = test_filter();
        let event = Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Metadata(
                notify::event::MetadataKind::Permissions,
            )),
            paths: vec![PathBuf::from("script.sh")],
            attrs: Default::default(),
        };
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], WatchEvent::Modified(_)));
    }

    #[test]
    fn notify_watcher_can_be_created() {
        use std::sync::Arc;

        let ignore = Arc::new(IgnoreFilter::default());
        let result = NotifyWatcher::new(ignore);
        assert!(result.is_ok());

        let (mut watcher, _rx) = result.unwrap();
        // Watch a valid temp dir.
        let dir = tempfile::TempDir::new().unwrap();
        assert!(watcher.watch(dir.path()).is_ok());
        assert!(watcher.unwatch(dir.path()).is_ok());
    }

    #[test]
    fn notify_watcher_watch_nonexistent_fails() {
        use std::sync::Arc;

        let ignore = Arc::new(IgnoreFilter::default());
        let (mut watcher, _rx) = NotifyWatcher::new(ignore).unwrap();
        let result = watcher.watch(Path::new("/no/such/directory/ever"));
        assert!(result.is_err());
    }

    #[test]
    fn notify_watcher_debug_impl() {
        use std::sync::Arc;
        let ignore = Arc::new(IgnoreFilter::default());
        let (watcher, _rx) = NotifyWatcher::new(ignore).unwrap();
        let debug = format!("{watcher:?}");
        assert!(debug.contains("NotifyWatcher"));
    }

    #[test]
    fn rescan_flag_produces_overflow() {
        use notify::event::Flag;

        let filter = test_filter();
        // inotify Q_OVERFLOW and FSEvents MUST_SCAN_SUBDIRS produce
        // EventKind::Other with Flag::Rescan and empty paths.
        let event = Event::new(EventKind::Other).set_flag(Flag::Rescan);
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], WatchEvent::Overflow));
    }

    #[test]
    fn rescan_flag_with_paths_still_produces_overflow() {
        use notify::event::Flag;

        let filter = test_filter();
        // Even if a rescan event carries paths, we still treat it as overflow
        // because the semantics are "everything may have changed".
        let mut event = Event::new(EventKind::Other).set_flag(Flag::Rescan);
        event.paths = vec![PathBuf::from("src/main.rs")];
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0], WatchEvent::Overflow));
    }

    #[test]
    fn mixed_paths_filter_individually() {
        let filter = test_filter();
        let event = Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Content,
            )),
            paths: vec![
                PathBuf::from("src/main.rs"),
                PathBuf::from("target/debug/binary"),
                PathBuf::from("src/lib.rs"),
            ],
            attrs: Default::default(),
        };
        let result = convert_event(&filter, &event);
        assert_eq!(result.len(), 2);
    }
}
