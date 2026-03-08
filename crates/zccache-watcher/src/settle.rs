//! Settle/coalesce buffer for bursty filesystem events.
//!
//! Filesystem watchers fire many events in rapid succession (e.g., a `cargo build`
//! touching 100 files in 10ms). The settle buffer waits for a configurable quiet
//! period before emitting a single coalesced batch.
//!
//! Overflow events bypass coalescing entirely — they clear pending state and
//! emit immediately, since everything is invalidated.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;

use crate::WatchEvent;

/// Output from the settle buffer.
#[derive(Debug, Clone)]
pub enum SettledEvent {
    /// A coalesced batch of file changes after the settle window.
    Batch {
        changed: Vec<PathBuf>,
        removed: Vec<PathBuf>,
    },
    /// Watcher overflow — all cached state should be considered stale.
    Overflow,
}

/// Coalesces bursty filesystem events into settled batches.
///
/// The daemon's file watcher produces a storm of events during builds.
/// The settle buffer absorbs them and waits for a quiet period (the settle
/// window) before emitting a single batch. This prevents the cache from
/// doing redundant work during a burst.
#[derive(Debug)]
pub struct SettleBuffer {
    settle_window: Duration,
}

/// Tracks the most recent change kind for a path during coalescing.
#[derive(Debug, Clone, Copy)]
enum ChangeKind {
    Modified,
    Removed,
}

impl SettleBuffer {
    /// Create a settle buffer with the given settle window.
    #[must_use]
    pub fn new(settle_window: Duration) -> Self {
        Self { settle_window }
    }

    /// Create a settle buffer with the default 50ms settle window.
    #[must_use]
    pub fn default_window() -> Self {
        Self::new(Duration::from_millis(50))
    }

    /// Run the settle loop.
    ///
    /// Reads raw `WatchEvent`s from `rx`, coalesces them, and sends
    /// `SettledEvent`s to `tx` after each settle window elapses.
    ///
    /// Returns when the input channel is closed.
    pub async fn run(
        &self,
        mut rx: mpsc::UnboundedReceiver<WatchEvent>,
        tx: mpsc::UnboundedSender<SettledEvent>,
    ) {
        let mut pending: HashMap<PathBuf, ChangeKind> = HashMap::new();

        loop {
            // Wait for the first event (or channel close).
            let event = match rx.recv().await {
                Some(e) => e,
                None => {
                    // Channel closed — flush any remaining events.
                    if !pending.is_empty() {
                        let _ = tx.send(Self::drain(&mut pending));
                    }
                    return;
                }
            };

            // Handle overflow immediately — don't wait for settle.
            if matches!(event, WatchEvent::Overflow) {
                pending.clear();
                let _ = tx.send(SettledEvent::Overflow);
                continue;
            }

            Self::apply_event(&mut pending, event);

            // Coalesce: keep reading until the settle window elapses with no new events.
            loop {
                match tokio::time::timeout(self.settle_window, rx.recv()).await {
                    Ok(Some(WatchEvent::Overflow)) => {
                        pending.clear();
                        let _ = tx.send(SettledEvent::Overflow);
                        break;
                    }
                    Ok(Some(event)) => {
                        Self::apply_event(&mut pending, event);
                    }
                    Ok(None) => {
                        // Channel closed — flush remaining.
                        if !pending.is_empty() {
                            let _ = tx.send(Self::drain(&mut pending));
                        }
                        return;
                    }
                    Err(_timeout) => {
                        // Settle window elapsed — emit batch.
                        if !pending.is_empty() {
                            let _ = tx.send(Self::drain(&mut pending));
                        }
                        break;
                    }
                }
            }
        }
    }

    fn apply_event(pending: &mut HashMap<PathBuf, ChangeKind>, event: WatchEvent) {
        match event {
            WatchEvent::Modified(p) | WatchEvent::Created(p) => {
                pending.insert(p, ChangeKind::Modified);
            }
            WatchEvent::Removed(p) => {
                pending.insert(p, ChangeKind::Removed);
            }
            WatchEvent::Renamed { from, to } => {
                pending.insert(from, ChangeKind::Removed);
                pending.insert(to, ChangeKind::Modified);
            }
            WatchEvent::Overflow | WatchEvent::Error(_) => {
                // Overflow handled in run(). Errors are logged upstream.
            }
        }
    }

    fn drain(pending: &mut HashMap<PathBuf, ChangeKind>) -> SettledEvent {
        let mut changed = Vec::new();
        let mut removed = Vec::new();
        for (path, kind) in pending.drain() {
            match kind {
                ChangeKind::Modified => changed.push(path),
                ChangeKind::Removed => removed.push(path),
            }
        }
        SettledEvent::Batch { changed, removed }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn single_event_settles() {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (settled_tx, mut settled_rx) = mpsc::unbounded_channel();

        let buffer = SettleBuffer::new(Duration::from_millis(20));
        let handle = tokio::spawn(async move {
            buffer.run(raw_rx, settled_tx).await;
        });

        raw_tx
            .send(WatchEvent::Modified(PathBuf::from("a.c")))
            .unwrap();
        drop(raw_tx);

        let event = settled_rx.recv().await.unwrap();
        match event {
            SettledEvent::Batch { changed, removed } => {
                assert_eq!(changed.len(), 1);
                assert!(removed.is_empty());
            }
            SettledEvent::Overflow => panic!("expected batch"),
        }

        handle.await.unwrap();
    }

    #[tokio::test]
    async fn rapid_events_coalesce_into_one_batch() {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (settled_tx, mut settled_rx) = mpsc::unbounded_channel();

        let buffer = SettleBuffer::new(Duration::from_millis(50));
        let handle = tokio::spawn(async move {
            buffer.run(raw_rx, settled_tx).await;
        });

        for i in 0..5 {
            raw_tx
                .send(WatchEvent::Modified(PathBuf::from(format!("file_{i}.c"))))
                .unwrap();
        }
        drop(raw_tx);

        let event = settled_rx.recv().await.unwrap();
        match event {
            SettledEvent::Batch { changed, removed } => {
                assert_eq!(changed.len(), 5);
                assert!(removed.is_empty());
            }
            SettledEvent::Overflow => panic!("expected batch"),
        }

        handle.await.unwrap();
    }

    #[tokio::test]
    async fn same_file_deduplicates() {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (settled_tx, mut settled_rx) = mpsc::unbounded_channel();

        let buffer = SettleBuffer::new(Duration::from_millis(20));
        let handle = tokio::spawn(async move {
            buffer.run(raw_rx, settled_tx).await;
        });

        let path = PathBuf::from("hot.c");
        raw_tx.send(WatchEvent::Modified(path.clone())).unwrap();
        raw_tx.send(WatchEvent::Modified(path.clone())).unwrap();
        raw_tx.send(WatchEvent::Modified(path)).unwrap();
        drop(raw_tx);

        let event = settled_rx.recv().await.unwrap();
        match event {
            SettledEvent::Batch { changed, removed } => {
                assert_eq!(changed.len(), 1);
                assert!(removed.is_empty());
            }
            SettledEvent::Overflow => panic!("expected batch"),
        }

        handle.await.unwrap();
    }

    #[tokio::test]
    async fn modify_then_remove_tracks_as_removed() {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (settled_tx, mut settled_rx) = mpsc::unbounded_channel();

        let buffer = SettleBuffer::new(Duration::from_millis(20));
        let handle = tokio::spawn(async move {
            buffer.run(raw_rx, settled_tx).await;
        });

        let path = PathBuf::from("temp.c");
        raw_tx.send(WatchEvent::Modified(path.clone())).unwrap();
        raw_tx.send(WatchEvent::Removed(path)).unwrap();
        drop(raw_tx);

        let event = settled_rx.recv().await.unwrap();
        match event {
            SettledEvent::Batch { changed, removed } => {
                assert!(changed.is_empty());
                assert_eq!(removed.len(), 1);
            }
            SettledEvent::Overflow => panic!("expected batch"),
        }

        handle.await.unwrap();
    }

    #[tokio::test]
    async fn remove_then_create_tracks_as_modified() {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (settled_tx, mut settled_rx) = mpsc::unbounded_channel();

        let buffer = SettleBuffer::new(Duration::from_millis(20));
        let handle = tokio::spawn(async move {
            buffer.run(raw_rx, settled_tx).await;
        });

        let path = PathBuf::from("replaced.c");
        raw_tx.send(WatchEvent::Removed(path.clone())).unwrap();
        raw_tx.send(WatchEvent::Created(path)).unwrap();
        drop(raw_tx);

        let event = settled_rx.recv().await.unwrap();
        match event {
            SettledEvent::Batch { changed, removed } => {
                assert_eq!(changed.len(), 1);
                assert!(changed.contains(&PathBuf::from("replaced.c")));
                assert!(removed.is_empty());
            }
            SettledEvent::Overflow => panic!("expected batch"),
        }

        handle.await.unwrap();
    }

    #[tokio::test]
    async fn rename_becomes_remove_and_modify() {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (settled_tx, mut settled_rx) = mpsc::unbounded_channel();

        let buffer = SettleBuffer::new(Duration::from_millis(20));
        let handle = tokio::spawn(async move {
            buffer.run(raw_rx, settled_tx).await;
        });

        raw_tx
            .send(WatchEvent::Renamed {
                from: PathBuf::from("old.c"),
                to: PathBuf::from("new.c"),
            })
            .unwrap();
        drop(raw_tx);

        let event = settled_rx.recv().await.unwrap();
        match event {
            SettledEvent::Batch { changed, removed } => {
                assert_eq!(changed.len(), 1);
                assert!(changed.contains(&PathBuf::from("new.c")));
                assert_eq!(removed.len(), 1);
                assert!(removed.contains(&PathBuf::from("old.c")));
            }
            SettledEvent::Overflow => panic!("expected batch"),
        }

        handle.await.unwrap();
    }

    #[tokio::test]
    async fn overflow_clears_pending_and_emits_immediately() {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (settled_tx, mut settled_rx) = mpsc::unbounded_channel();

        let buffer = SettleBuffer::new(Duration::from_millis(50));
        let handle = tokio::spawn(async move {
            buffer.run(raw_rx, settled_tx).await;
        });

        // Send some events, then overflow.
        raw_tx
            .send(WatchEvent::Modified(PathBuf::from("a.c")))
            .unwrap();
        raw_tx.send(WatchEvent::Overflow).unwrap();
        drop(raw_tx);

        let event = settled_rx.recv().await.unwrap();
        assert!(matches!(event, SettledEvent::Overflow));

        handle.await.unwrap();
    }

    #[tokio::test]
    async fn error_events_are_ignored() {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (settled_tx, mut settled_rx) = mpsc::unbounded_channel();

        let buffer = SettleBuffer::new(Duration::from_millis(20));
        let handle = tokio::spawn(async move {
            buffer.run(raw_rx, settled_tx).await;
        });

        raw_tx
            .send(WatchEvent::Modified(PathBuf::from("a.c")))
            .unwrap();
        raw_tx
            .send(WatchEvent::Error("some error".to_string()))
            .unwrap();
        drop(raw_tx);

        let event = settled_rx.recv().await.unwrap();
        match event {
            SettledEvent::Batch { changed, removed } => {
                assert_eq!(changed.len(), 1);
                assert!(removed.is_empty());
            }
            SettledEvent::Overflow => panic!("expected batch"),
        }

        handle.await.unwrap();
    }

    #[tokio::test]
    async fn default_window_creates_buffer() {
        let buffer = SettleBuffer::default_window();
        // Just verify it can run without panicking.
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (settled_tx, mut settled_rx) = mpsc::unbounded_channel();

        let handle = tokio::spawn(async move {
            buffer.run(raw_rx, settled_tx).await;
        });

        raw_tx
            .send(WatchEvent::Modified(PathBuf::from("x.c")))
            .unwrap();
        drop(raw_tx);

        let event = settled_rx.recv().await.unwrap();
        assert!(matches!(event, SettledEvent::Batch { .. }));
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn multiple_overflows_in_sequence() {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (settled_tx, mut settled_rx) = mpsc::unbounded_channel();

        let buffer = SettleBuffer::new(Duration::from_millis(20));
        let handle = tokio::spawn(async move {
            buffer.run(raw_rx, settled_tx).await;
        });

        raw_tx.send(WatchEvent::Overflow).unwrap();
        raw_tx.send(WatchEvent::Overflow).unwrap();
        raw_tx.send(WatchEvent::Overflow).unwrap();
        drop(raw_tx);

        // Should get at least one overflow event.
        let mut overflow_count = 0;
        while let Some(event) = settled_rx.recv().await {
            if matches!(event, SettledEvent::Overflow) {
                overflow_count += 1;
            }
        }
        assert!(overflow_count >= 1);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn overflow_then_normal_events() {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (settled_tx, mut settled_rx) = mpsc::unbounded_channel();

        let buffer = SettleBuffer::new(Duration::from_millis(20));
        let handle = tokio::spawn(async move {
            buffer.run(raw_rx, settled_tx).await;
        });

        raw_tx.send(WatchEvent::Overflow).unwrap();
        raw_tx
            .send(WatchEvent::Modified(PathBuf::from("after.c")))
            .unwrap();
        drop(raw_tx);

        let mut saw_overflow = false;
        let mut saw_batch = false;
        while let Some(event) = settled_rx.recv().await {
            match event {
                SettledEvent::Overflow => saw_overflow = true,
                SettledEvent::Batch { changed, .. } => {
                    assert!(changed.contains(&PathBuf::from("after.c")));
                    saw_batch = true;
                }
            }
        }
        assert!(saw_overflow);
        assert!(saw_batch);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn large_batch_coalesces() {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (settled_tx, mut settled_rx) = mpsc::unbounded_channel();

        let buffer = SettleBuffer::new(Duration::from_millis(50));
        let handle = tokio::spawn(async move {
            buffer.run(raw_rx, settled_tx).await;
        });

        for i in 0..200 {
            raw_tx
                .send(WatchEvent::Modified(PathBuf::from(format!(
                    "src/file_{i}.c"
                ))))
                .unwrap();
        }
        drop(raw_tx);

        // Collect all batches — total changed files should be 200.
        let mut total_changed = 0;
        while let Some(event) = settled_rx.recv().await {
            if let SettledEvent::Batch { changed, .. } = event {
                total_changed += changed.len();
            }
        }
        assert_eq!(total_changed, 200);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn mixed_event_types_in_burst() {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (settled_tx, mut settled_rx) = mpsc::unbounded_channel();

        let buffer = SettleBuffer::new(Duration::from_millis(20));
        let handle = tokio::spawn(async move {
            buffer.run(raw_rx, settled_tx).await;
        });

        raw_tx
            .send(WatchEvent::Created(PathBuf::from("new.c")))
            .unwrap();
        raw_tx
            .send(WatchEvent::Modified(PathBuf::from("edit.c")))
            .unwrap();
        raw_tx
            .send(WatchEvent::Removed(PathBuf::from("gone.c")))
            .unwrap();
        raw_tx
            .send(WatchEvent::Renamed {
                from: PathBuf::from("old.c"),
                to: PathBuf::from("renamed.c"),
            })
            .unwrap();
        raw_tx
            .send(WatchEvent::Error("ignored".to_string()))
            .unwrap();
        drop(raw_tx);

        let event = settled_rx.recv().await.unwrap();
        match event {
            SettledEvent::Batch { changed, removed } => {
                // new.c (Created), edit.c (Modified), renamed.c (from Renamed.to)
                assert_eq!(changed.len(), 3);
                // gone.c (Removed), old.c (from Renamed.from)
                assert_eq!(removed.len(), 2);
            }
            SettledEvent::Overflow => panic!("expected batch"),
        }
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn channel_close_mid_coalesce_flushes() {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (settled_tx, mut settled_rx) = mpsc::unbounded_channel();

        let buffer = SettleBuffer::new(Duration::from_millis(500)); // Long window
        let handle = tokio::spawn(async move {
            buffer.run(raw_rx, settled_tx).await;
        });

        // Send events then close channel before settle window elapses.
        raw_tx
            .send(WatchEvent::Modified(PathBuf::from("a.c")))
            .unwrap();
        raw_tx
            .send(WatchEvent::Modified(PathBuf::from("b.c")))
            .unwrap();
        drop(raw_tx); // Close before 500ms window

        let event = settled_rx.recv().await.unwrap();
        match event {
            SettledEvent::Batch { changed, .. } => {
                assert_eq!(changed.len(), 2);
            }
            SettledEvent::Overflow => panic!("expected batch"),
        }
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn overflow_with_empty_pending() {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (settled_tx, mut settled_rx) = mpsc::unbounded_channel();

        let buffer = SettleBuffer::new(Duration::from_millis(20));
        let handle = tokio::spawn(async move {
            buffer.run(raw_rx, settled_tx).await;
        });

        // Overflow with no prior events.
        raw_tx.send(WatchEvent::Overflow).unwrap();
        drop(raw_tx);

        let event = settled_rx.recv().await.unwrap();
        assert!(matches!(event, SettledEvent::Overflow));
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn empty_close_produces_no_output() {
        let (raw_tx, raw_rx) = mpsc::unbounded_channel();
        let (settled_tx, mut settled_rx) = mpsc::unbounded_channel();

        let buffer = SettleBuffer::new(Duration::from_millis(20));
        let handle = tokio::spawn(async move {
            buffer.run(raw_rx, settled_tx).await;
        });

        drop(raw_tx);

        // Should get None (channel closed, no events).
        let event = settled_rx.recv().await;
        assert!(event.is_none());

        handle.await.unwrap();
    }
}
