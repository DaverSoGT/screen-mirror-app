// Cancel-gate tests for the sender rebuild worker (Batch 5, T5.1).
//
// Each test exercises ONE specific cancel gate (A/B/C/D) in `make_sender_rebuild_hook`.
// All tests use controlled-timing: channel-based blocks with `recv_timeout(5s)` and
// explicit thread join to avoid CI flake.
//
// Gate A — before teardown (stop arrived before worker started any work).
// Gate B — after teardown, before builder invocation.
// Gate C — after builder returns Ok, before session swap.
// Gate D — after session swap, before RebuildSucceeded is signalled.
//
// Test strategy:
//   - Create a fake "old" session with an injectable shutdown closure.
//   - Use a blocking fake builder controlled by a SyncSender<()>.
//   - Set old_stop_flag at the exact point under test, then release the block.
//   - Assert the signal_tx receives RebuildFailed (not RebuildSucceeded).

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use screen_mirror_lib::commands::sender::{
    BundleError, ChannelLike, RestartCache, SenderBundle, SenderCounters, SenderSession,
    make_sender_rebuild_hook,
};
use sm_domain::supervisor::SupervisorSignal;

// ─── Minimal ChannelLike stub ─────────────────────────────────────────────────

struct NullChannel;
impl ChannelLike for NullChannel {
    fn send_raw(&self, _discriminant: u8, _bytes: Vec<u8>) -> Result<(), String> {
        Ok(())
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Create a minimal RestartCache so the worker can read construction params.
fn make_cache(channel: Arc<dyn ChannelLike>) -> Arc<Mutex<Option<RestartCache>>> {
    let cache = RestartCache {
        udp_port: 0,
        service_name: "_test._tcp.local.".to_string(),
        session_nonce: 1,
        channel,
    };
    Arc::new(Mutex::new(Some(cache)))
}

/// Create an empty session arc (no session installed — teardown is a no-op).
fn empty_session() -> Arc<Mutex<Option<SenderSession>>> {
    Arc::new(Mutex::new(None))
}

/// Create a session arc with a custom blocking shutdown closure.
/// `release_rx` unblocks the shutdown when the test sends on `release_tx`.
fn blocking_session(
    old_stop_flag: Arc<AtomicBool>,
    channel: Arc<dyn ChannelLike>,
    release_rx: std::sync::mpsc::Receiver<()>,
) -> Arc<Mutex<Option<SenderSession>>> {
    let session = SenderSession::new(
        old_stop_flag,
        vec![],
        channel,
        Arc::new(SenderCounters::default()),
        Some(Box::new(move || {
            // Block until the test releases us.
            let _ = release_rx.recv_timeout(Duration::from_secs(5));
        })),
        "sw_fake".to_string(),
        None, // D-6: suppress_bye_on_rebuild — None for test stubs
        None, // D-RFG: stop_signaling_on_rebuild — None for test stubs
        None, // D-RFG-6: disarm_escalation_on_rebuild — None for test stubs
    );
    Arc::new(Mutex::new(Some(session)))
}

// ─── Gate A: stop before worker starts any work ───────────────────────────────

/// Gate A fires when old_stop_flag is true before the hook is even called.
/// The worker observes the flag immediately on entry and signals RebuildFailed
/// without performing any teardown or builder invocation.
#[test]
fn cancel_gate_a_stop_before_hook_fires_rebuild_failed() {
    let old_stop_flag = Arc::new(AtomicBool::new(false));
    let ch: Arc<dyn ChannelLike> = Arc::new(NullChannel);
    let bridge_session = empty_session();
    let bridge_cache = make_cache(ch.clone());

    // Builder must NOT be called — it panics if called.
    let builder = Arc::new(move |_, _, _, _, _| -> Result<SenderBundle, BundleError> {
        panic!("Gate A: builder must not be called when stop_flag is already set");
    });

    let hook = make_sender_rebuild_hook(
        builder,
        bridge_cache,
        bridge_session,
        old_stop_flag.clone(),
        1,
        Arc::new(AtomicU8::new(1)), // T1.10: default epoch — test doesn't drive epoch
    );

    let (signal_tx, signal_rx) = sync_channel::<SupervisorSignal>(4);

    // Set the stop flag BEFORE calling the hook.
    old_stop_flag.store(true, Ordering::Relaxed);

    let hook_handle = thread::Builder::new()
        .name("test-hook-gate-a".into())
        .spawn(move || hook(signal_tx))
        .unwrap();

    // The worker should emit RebuildFailed quickly.
    let outcome = signal_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("expected a signal within 5s");

    hook_handle.join().expect("hook thread must not panic");

    assert!(
        matches!(outcome, SupervisorSignal::RebuildFailed),
        "Gate A must produce RebuildFailed, got: {outcome:?}"
    );
}

// ─── Gate B: stop after teardown, before builder invocation ──────────────────

/// Gate B fires when old_stop_flag becomes true DURING the shutdown closure
/// (while teardown is in progress). The worker completes teardown, then checks
/// the flag before calling the builder → RebuildFailed, builder NOT called.
#[test]
fn cancel_gate_b_stop_during_teardown_fires_rebuild_failed() {
    let old_stop_flag = Arc::new(AtomicBool::new(false));
    let ch: Arc<dyn ChannelLike> = Arc::new(NullChannel);
    let bridge_cache = make_cache(ch.clone());

    // Channel that unblocks the shutdown closure.
    let (release_tx, release_rx) = sync_channel::<()>(1);

    let bridge_session = blocking_session(old_stop_flag.clone(), ch.clone(), release_rx);

    let builder_called = Arc::new(AtomicBool::new(false));
    let builder_called_for_check = builder_called.clone();
    let builder = Arc::new(move |_, _, _, _, _| -> Result<SenderBundle, BundleError> {
        builder_called_for_check.store(true, Ordering::Relaxed);
        Ok(SenderBundle::test_stub())
    });

    let hook = make_sender_rebuild_hook(
        builder,
        bridge_cache,
        bridge_session,
        old_stop_flag.clone(),
        1,
        Arc::new(AtomicU8::new(1)), // T1.10: default epoch — test doesn't drive epoch
    );

    let (signal_tx, signal_rx) = sync_channel::<SupervisorSignal>(4);

    // Spawn hook (stop_flag starts false → Gate A passes).
    let hook_handle = thread::Builder::new()
        .name("test-hook-gate-b".into())
        .spawn(move || hook(signal_tx))
        .unwrap();

    // Give worker time to reach the shutdown closure (blocked on release_rx).
    thread::sleep(Duration::from_millis(20));

    // Set stop_flag WHILE teardown is in progress.
    old_stop_flag.store(true, Ordering::Relaxed);

    // Release the shutdown closure.
    let _ = release_tx.send(());

    // Worker should signal RebuildFailed after Gate B fires.
    let outcome = signal_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("expected a signal within 5s");

    hook_handle.join().expect("hook thread must not panic");

    assert!(
        matches!(outcome, SupervisorSignal::RebuildFailed),
        "Gate B must produce RebuildFailed, got: {outcome:?}"
    );
    assert!(
        !builder_called.load(Ordering::Relaxed),
        "Gate B must fire before builder is called"
    );
}

// ─── Gate C: stop after builder returns Ok, before session swap ───────────────

/// Gate C fires when old_stop_flag becomes true DURING builder execution.
/// The worker receives the new bundle, checks the flag, sees it set, tears down
/// the freshly-built bundle (calls its shutdown), and signals RebuildFailed.
#[test]
fn cancel_gate_c_stop_during_build_fires_rebuild_failed() {
    let old_stop_flag = Arc::new(AtomicBool::new(false));
    let ch: Arc<dyn ChannelLike> = Arc::new(NullChannel);
    let bridge_session = empty_session();
    let bridge_cache = make_cache(ch.clone());

    // Channel that unblocks the builder. Wrapped in Mutex so the closure is Sync.
    let (release_tx, release_rx) = sync_channel::<()>(1);
    let release_rx = Arc::new(Mutex::new(release_rx));

    let release_rx_for_builder = release_rx.clone();
    let builder = Arc::new(move |_, _, _, _, _| -> Result<SenderBundle, BundleError> {
        // Block until the test releases us.
        let _ = release_rx_for_builder
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(5));
        Ok(SenderBundle::test_stub())
    });

    let hook = make_sender_rebuild_hook(
        builder,
        bridge_cache,
        bridge_session.clone(),
        old_stop_flag.clone(),
        1,
        Arc::new(AtomicU8::new(1)), // T1.10: default epoch — test doesn't drive epoch
    );

    let (signal_tx, signal_rx) = sync_channel::<SupervisorSignal>(4);

    // Spawn hook — Gate A passes (stop_flag false), teardown is a no-op (empty_session),
    // Gate B passes (stop_flag false), builder starts and blocks on release_rx.
    let hook_handle = thread::Builder::new()
        .name("test-hook-gate-c".into())
        .spawn(move || hook(signal_tx))
        .unwrap();

    // Give worker time to reach the builder block.
    thread::sleep(Duration::from_millis(20));

    // Set stop_flag while builder is still blocked.
    old_stop_flag.store(true, Ordering::Relaxed);

    // Release the builder.
    let _ = release_tx.send(());

    // Worker: builder returns Ok → Gate C fires (stop_flag true) → RebuildFailed.
    let outcome = signal_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("expected a signal within 5s");

    hook_handle.join().expect("hook thread must not panic");

    assert!(
        matches!(outcome, SupervisorSignal::RebuildFailed),
        "Gate C must produce RebuildFailed, got: {outcome:?}"
    );

    // Bridge session must remain empty (swap must NOT have happened).
    assert!(
        bridge_session.lock().unwrap().is_none(),
        "Gate C must not install the new session into bridge_session"
    );
}

// ─── Gate D: stop after session swap, before RebuildSucceeded ─────────────────

/// Gate D fires when old_stop_flag becomes true AFTER the swap but before
/// RebuildSucceeded is sent. The worker tears down the newly-installed session
/// and signals RebuildFailed.
///
/// Controlled-timing technique:
/// 1. Builder signals "started" via a channel then blocks on release_rx.
/// 2. Test waits for that signal, then acquires bridge_session.lock() (step 6
///    has already released the lock by the time the builder runs).
/// 3. Test releases the builder, then sets stop_flag=true.
/// 4. Worker: Gate C passes (stop_flag was false when Gate C checked), then
///    tries bridge_session.lock() at Step 11 → BLOCKS (test holds it).
/// 5. Test releases the lock — worker swaps, hits Gate D (stop_flag true),
///    tears down the new session, signals RebuildFailed.
///
/// This sequence is deterministic: test holds the lock from before the builder
/// returns until after stop_flag is set, so Gate C always passes and Gate D
/// always fires.
#[test]
fn cancel_gate_d_stop_during_swap_fires_rebuild_failed() {
    let old_stop_flag = Arc::new(AtomicBool::new(false));
    let ch: Arc<dyn ChannelLike> = Arc::new(NullChannel);
    let bridge_session = empty_session();
    let bridge_cache = make_cache(ch.clone());

    // Two-channel builder:
    //   builder_started_tx — sent when builder is about to block.
    //   release_rx — builder blocks until test releases.
    let (builder_started_tx, builder_started_rx) = sync_channel::<()>(1);
    let (release_tx, release_rx) = sync_channel::<()>(1);
    let release_rx = Arc::new(Mutex::new(release_rx));

    let builder = Arc::new(move |_, _, _, _, _| -> Result<SenderBundle, BundleError> {
        // Notify test that builder has started and is about to block.
        let _ = builder_started_tx.send(());
        // Block until test releases us.
        let _ = release_rx
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(5));
        Ok(SenderBundle::test_stub())
    });

    let hook = make_sender_rebuild_hook(
        builder,
        bridge_cache,
        bridge_session.clone(),
        old_stop_flag.clone(),
        1,
        Arc::new(AtomicU8::new(1)), // T1.10: default epoch — test doesn't drive epoch
    );

    let (signal_tx, signal_rx) = sync_channel::<SupervisorSignal>(4);

    // Spawn the hook — it will proceed through Gate A, step 6 (releases lock
    // after take), Gate B (stop_flag=false), then enter the blocking builder.
    let hook_handle = thread::Builder::new()
        .name("test-hook-gate-d".into())
        .spawn(move || hook(signal_tx))
        .unwrap();

    // Wait for the builder to start (which means step 6 + Gate B have already
    // completed and the bridge_session lock is available again).
    builder_started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("builder must start within 5s");

    // Acquire bridge_session.lock(): step 6 has already released it,
    // and the worker is blocked inside the builder (not at step 11 yet).
    let guard = bridge_session.lock().unwrap();

    // Release the builder so it can return Ok and proceed to Gate C → step 11.
    // stop_flag is still FALSE here, so Gate C passes.
    let _ = release_tx.send(());

    // Give the worker time to:
    //   - receive the release signal (instant)
    //   - pass Gate C check (instant, stop_flag=false)
    //   - call bridge_session.lock().unwrap() at Step 11 → BLOCK (we hold guard)
    // 5ms is a generous margin for these sub-millisecond operations.
    thread::sleep(Duration::from_millis(5));

    // Set stop_flag=true while worker is blocked at step 11 (bridge_session held).
    old_stop_flag.store(true, Ordering::Relaxed);

    // Release the lock — worker swaps, hits Gate D (stop_flag true), tears down,
    // signals RebuildFailed.
    drop(guard);

    let outcome = signal_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("expected a signal within 5s");

    hook_handle.join().expect("hook thread must not panic");

    assert!(
        matches!(outcome, SupervisorSignal::RebuildFailed),
        "Gate D must produce RebuildFailed, got: {outcome:?}"
    );

    // Bridge session must be empty after Gate D tears down the newly-swapped session.
    assert!(
        bridge_session.lock().unwrap().is_none(),
        "Gate D must tear down the newly-installed session (bridge_session must be None)"
    );
}
