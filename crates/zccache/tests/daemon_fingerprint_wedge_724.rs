//! End-to-end daemon load repro for issue #724 (fingerprint check wedge).
//!
//! Drives the real `DaemonServer` over IPC on an isolated endpoint. One client
//! fires a `FingerprintCheck` over a large watched tree whose files all changed
//! — the daemon must re-stat + re-hash every one. Concurrently, a second client
//! hammers `FingerprintMarkSuccess` on a nonexistent cache file: on the daemon
//! side that is a full `iter_mut` sweep over every fingerprint-map shard.
//!
//! Before the fix, `check()` held a DashMap write-shard lock across the whole
//! re-hash, so the concurrent sweep blocked for the entire verify — under heavy
//! parallel cargo that stall propagated across handlers and wedged the daemon
//! past the client's timeout. After the fix the re-hash runs off-lock, so the
//! sweep stays responsive while the verify is in flight.
//!
//! Uses a multi-thread runtime to mirror the production daemon: the verify and
//! the sweep run on distinct workers, so the ONLY thing that can stall the
//! sweep is the fingerprint shard lock — which is exactly what the fix releases.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result
)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use zccache::daemon::DaemonServer;
use zccache::protocol::{Request, Response};

async fn start_daemon() -> (
    String,
    tokio::task::JoinHandle<()>,
    std::sync::Arc<tokio::sync::Notify>,
) {
    let endpoint = zccache::ipc::unique_test_endpoint();
    let mut server = DaemonServer::bind(&endpoint).unwrap();
    let shutdown = server.shutdown_handle();
    let handle = tokio::spawn(async move {
        server.run(0).await.unwrap();
    });
    (endpoint, handle, shutdown)
}

async fn fingerprint_check(
    endpoint: &str,
    cache_file: &std::path::Path,
    root: &std::path::Path,
) -> String {
    let mut client = zccache::ipc::connect(endpoint).await.unwrap();
    client
        .send(&Request::FingerprintCheck {
            cache_file: cache_file.to_path_buf().into(),
            cache_type: "two-layer".into(),
            root: root.to_path_buf().into(),
            extensions: vec![],
            include_globs: vec![],
            exclude: vec![],
        })
        .await
        .unwrap();
    match client.recv().await.unwrap() {
        Some(Response::FingerprintCheckResult { decision, .. }) => decision,
        other => panic!("unexpected response: {other:?}"),
    }
}

async fn mark_success(client: &mut zccache::ipc::IpcConnection, cache_file: &std::path::Path) {
    client
        .send(&Request::FingerprintMarkSuccess {
            cache_file: cache_file.to_path_buf().into(),
        })
        .await
        .unwrap();
    assert_eq!(client.recv().await.unwrap(), Some(Response::FingerprintAck));
}

// 16 workers mirrors the real daemon (one worker per core, 32 on the dev boxes).
// OS-thread preemption — not free worker slots — is what keeps the concurrent
// sweep scheduled while the verify's synchronous re-hash runs, so the ONLY thing
// that can stall the sweep is the fingerprint shard lock, which is exactly what
// the fix stops holding across the re-hash. Not #[ignore]d: it runs in the
// serial (`--test-threads=1`) integration job; the in-process daemon is
// self-contained and bounded by `test_timeout` (30 s).
#[tokio::test(flavor = "multi_thread", worker_threads = 16)]
async fn fingerprint_check_verify_does_not_wedge_concurrent_requests() {
    zccache::test_support::test_timeout(async {
        const FILES: usize = 800;

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let src = tempfile::TempDir::new().unwrap();
        let cache_dir = tempfile::TempDir::new().unwrap();
        let cache_file = cache_dir.path().join("fp.json");

        let blob_a = vec![b'a'; 16 * 1024];
        let blob_b = vec![b'b'; 16 * 1024];
        for i in 0..FILES {
            std::fs::write(src.path().join(format!("f{i}.c")), &blob_a).unwrap();
        }

        // Prime the watch and mark it clean so the next check takes the verify
        // branch (the one that re-stats + re-hashes).
        let mut primer = zccache::ipc::connect(&endpoint).await.unwrap();
        assert_eq!(
            fingerprint_check(&endpoint, &cache_file, src.path()).await,
            "run"
        );
        mark_success(&mut primer, &cache_file).await;

        // Change every file so the verify must re-hash the whole tree.
        std::thread::sleep(Duration::from_millis(50));
        for i in 0..FILES {
            std::fs::write(src.path().join(format!("f{i}.c")), &blob_b).unwrap();
        }

        // Fire the big verify on its own connection.
        let done = Arc::new(AtomicBool::new(false));
        let verify_done = Arc::clone(&done);
        let verify_ep = endpoint.clone();
        let verify_cache = cache_file.clone();
        let verify_root = src.path().to_path_buf();
        let verifier = tokio::spawn(async move {
            let t = Instant::now();
            let decision = fingerprint_check(&verify_ep, &verify_cache, &verify_root).await;
            verify_done.store(true, Ordering::Release);
            (t.elapsed(), decision)
        });

        // Hammer a full-shard sweep (mark_success on a nonexistent cache file)
        // until the verify finishes, tracking the slowest round-trip — the time
        // a sweep was blocked behind the shard lock.
        let mut sweeper = zccache::ipc::connect(&endpoint).await.unwrap();
        let nonexistent = cache_dir.path().join("no-such-watch.json");
        let mut max_sweep = Duration::ZERO;
        let mut sweeps = 0u64;
        let mut slow_sweeps = 0u64;
        while !done.load(Ordering::Acquire) {
            let t = Instant::now();
            mark_success(&mut sweeper, &nonexistent).await;
            let rtt = t.elapsed();
            max_sweep = max_sweep.max(rtt);
            if rtt > Duration::from_millis(10) {
                slow_sweeps += 1;
            }
            sweeps += 1;
            tokio::task::yield_now().await;
        }

        let (verify_dur, decision) = verifier.await.unwrap();
        eprintln!(
            "issue #724 e2e: verify={verify_dur:?} sweeps={sweeps} slow(>10ms)={slow_sweeps} \
             max_sweep_rtt={max_sweep:?} ({FILES} files re-hashed)"
        );
        assert_eq!(decision, "run", "changed tree must be detected");
        assert!(
            verify_dur >= Duration::from_millis(3),
            "verify too fast ({verify_dur:?}) to exercise contention — grow the fixture"
        );

        // The wedge signature is loss of THROUGHPUT, not a single slow call: when
        // the verify holds the shard lock across the whole re-hash, every sweep's
        // `iter_mut` blocks until the verify ends, so the sweeper completes only a
        // handful of (all-slow) round-trips. With the lock released the sweeper
        // stays responsive and completes hundreds — the verify runs concurrently.
        assert!(
            sweeps >= 50,
            "sweeper completed only {sweeps} round-trips during the {verify_dur:?} verify — \
             the daemon wedged concurrent fingerprint requests behind the re-hash (issue #724)"
        );
        // And the vast majority stay fast (allow a small transient tail).
        assert!(
            slow_sweeps * 20 <= sweeps,
            "{slow_sweeps}/{sweeps} sweep round-trips were slow (>10ms) during the verify — \
             concurrent fingerprint requests are stalling behind the re-hash (issue #724)"
        );

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}
