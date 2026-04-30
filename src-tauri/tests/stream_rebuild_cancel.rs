// Cancel-gate tests for the stream rebuild worker (Batch 5, T5.3).
//
// Each test exercises ONE specific cancel gate (A/B/C/D) in `make_stream_rebuild_hook`.
// All tests use controlled-timing: channel-based blocks with `recv_timeout(5s)` and
// explicit thread join to avoid CI flake.
//
// Gate A — before teardown (stop arrived before worker started any work).
// Gate B — after teardown + bind_probe, before builder invocation.
// Gate C — after builder returns Ok, before session swap.
// Gate D — after session swap, before RebuildSucceeded is signalled.
//
// Asymmetry vs sender: the stream worker calls `bind_probe(port)` between teardown
// and Gate B. Tests inject a `ProbeFn` returning a real socket from port 0 (ephemeral)
// so the bind_probe step succeeds quickly.
//
// Test strategy:
//   - Create a fake "old" stream session with an injectable signaling/teardown hook.
//   - Use a blocking fake builder controlled by a SyncSender<()>.
//   - Set old_stop_flag at the exact point under test, then release the block.
//   - Assert the signal_tx receives RebuildFailed (not RebuildSucceeded).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use screen_mirror_lib::commands::sender::ChannelLike;
use screen_mirror_lib::commands::stream::{
    BindCtx, BundleError, ProbeFn, ReceiverBundle, ReceiverOps, StreamRestartCache,
    make_stream_rebuild_hook,
};
use sm_domain::supervisor::SupervisorSignal;
use sm_domain::transport::TransportError;

// ─── Minimal stubs ─────────────────────────────────��──────────────────────────

struct NullChannel;
impl ChannelLike for NullChannel {
    fn send_raw(&self, _discriminant: u8, _bytes: Vec<u8>) -> Result<(), String> {
        Ok(())
    }
}

struct FakeReceiverOps;
impl ReceiverOps for FakeReceiverOps {
    fn request_keyframe(&self) -> Result<(), TransportError> {
        Ok(())
    }
    fn dropped_frames(&self) -> u64 {
        0
    }
}

// ─── Helpers ────────────────────────────��───────────────────────────��────────

/// A ProbeFn that immediately binds an ephemeral UDP socket (port 0).
/// Used so the bind_probe step succeeds without needing real port management.
fn instant_probe_fn() -> ProbeFn {
    Arc::new(|_port: u16| {
        std::net::UdpSocket::bind("127.0.0.1:0").map_err(|e| BundleError::Other(e.to_string()))
    })
}

/// Create a minimal StreamRestartCache.
fn make_cache(channel: Arc<dyn ChannelLike>) -> Arc<Mutex<Option<StreamRestartCache>>> {
    let cache = StreamRestartCache {
        udp_port: 0,
        service_name: "_test._tcp.local.".to_string(),
        channel,
        session_nonce: 1,
    };
    Arc::new(Mutex::new(Some(cache)))
}

/// Create an empty stream session arc (no session installed — teardown is a no-op).
fn empty_session() -> Arc<Mutex<Option<screen_mirror_lib::commands::stream::StreamSession>>> {
    Arc::new(Mutex::new(None))
}

/// Build a minimal fake ReceiverBundle for use in builder closures.
fn fake_bundle() -> ReceiverBundle {
    let (_pkt_tx, pkt_rx) = sync_channel::<sm_domain::encode::EncodedPacket>(1);
    ReceiverBundle {
        receiver: Box::new(FakeReceiverOps),
        pkt_rx,
        signaling: None,
        drain_handles: vec![],
        _drain_senders: vec![],
    }
}

// ─── Gate A: stop before worker starts any work ───────────────────────────────

/// Gate A fires when old_stop_flag is true before the hook is even called.
/// The worker observes the flag immediately on entry and signals RebuildFailed.
#[test]
fn cancel_gate_a_stop_before_hook_fires_rebuild_failed() {
    let old_stop_flag = Arc::new(AtomicBool::new(false));
    let ch: Arc<dyn ChannelLike> = Arc::new(NullChannel);
    let bridge_session = empty_session();
    let bridge_cache = make_cache(ch.clone());

    // Builder must NOT be called — panics if called.
    let builder = Arc::new(
        move |_bind_ctx: BindCtx,
              _port: u16,
              _name: String,
              _stop_flag: Arc<AtomicBool>,
              _channel: Arc<dyn ChannelLike>|
              -> Result<ReceiverBundle, BundleError> {
            panic!("Gate A: builder must not be called when stop_flag is already set");
        },
    );

    let hook = make_stream_rebuild_hook(
        builder,
        bridge_cache,
        bridge_session,
        old_stop_flag.clone(),
        1,
        Some(instant_probe_fn()),
    );

    let (signal_tx, signal_rx) = sync_channel::<SupervisorSignal>(4);

    // Set the stop flag BEFORE calling the hook.
    old_stop_flag.store(true, Ordering::Relaxed);

    let hook_handle = thread::Builder::new()
        .name("test-stream-hook-gate-a".into())
        .spawn(move || hook(signal_tx))
        .unwrap();

    let outcome = signal_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("expected a signal within 5s");

    hook_handle.join().expect("hook thread must not panic");

    assert!(
        matches!(outcome, SupervisorSignal::RebuildFailed),
        "Gate A must produce RebuildFailed, got: {outcome:?}"
    );
}

// ─── Gate B: stop after teardown + bind_probe, before builder ─────────────────

/// Gate B fires when old_stop_flag becomes true after teardown and bind_probe
/// but before the builder is called.
///
/// Controlled-timing: the probe_fn blocks until released, giving the test time
/// to set the stop_flag before the probe returns (and before Gate B is checked).
#[test]
fn cancel_gate_b_stop_after_teardown_before_build_fires_rebuild_failed() {
    let old_stop_flag = Arc::new(AtomicBool::new(false));
    let ch: Arc<dyn ChannelLike> = Arc::new(NullChannel);
    let bridge_session = empty_session();
    let bridge_cache = make_cache(ch.clone());

    // Blocking probe_fn: blocks until released, then returns a socket.
    let (probe_release_tx, probe_release_rx) = sync_channel::<()>(1);
    let probe_release_rx = Arc::new(Mutex::new(probe_release_rx));

    let blocking_probe: ProbeFn = Arc::new(move |_port: u16| {
        // Block until released.
        let _ = probe_release_rx
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(5));
        std::net::UdpSocket::bind("127.0.0.1:0").map_err(|e| BundleError::Other(e.to_string()))
    });

    let builder_called = Arc::new(AtomicBool::new(false));
    let builder_called_for_check = builder_called.clone();
    let builder = Arc::new(
        move |_bind_ctx: BindCtx,
              _port: u16,
              _name: String,
              _stop_flag: Arc<AtomicBool>,
              _channel: Arc<dyn ChannelLike>|
              -> Result<ReceiverBundle, BundleError> {
            builder_called_for_check.store(true, Ordering::Relaxed);
            Ok(fake_bundle())
        },
    );

    let hook = make_stream_rebuild_hook(
        builder,
        bridge_cache,
        bridge_session,
        old_stop_flag.clone(),
        1,
        Some(blocking_probe),
    );

    let (signal_tx, signal_rx) = sync_channel::<SupervisorSignal>(4);

    // Spawn hook — Gate A passes (stop_flag false), teardown is a no-op,
    // then worker reaches the blocking probe_fn.
    let hook_handle = thread::Builder::new()
        .name("test-stream-hook-gate-b".into())
        .spawn(move || hook(signal_tx))
        .unwrap();

    // Give worker time to reach the blocking probe.
    thread::sleep(Duration::from_millis(20));

    // Set stop_flag WHILE probe is blocked (simulating stop during teardown window).
    old_stop_flag.store(true, Ordering::Relaxed);

    // Release the probe — it returns a socket.
    let _ = probe_release_tx.send(());

    // Worker: probe returns → Gate B checks stop_flag (true) → RebuildFailed.
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
/// The worker receives the new bundle, checks the flag, and signals RebuildFailed.
#[test]
fn cancel_gate_c_stop_during_build_fires_rebuild_failed() {
    let old_stop_flag = Arc::new(AtomicBool::new(false));
    let ch: Arc<dyn ChannelLike> = Arc::new(NullChannel);
    let bridge_session = empty_session();
    let bridge_cache = make_cache(ch.clone());

    // Blocking builder: blocks until released.
    let (release_tx, release_rx) = sync_channel::<()>(1);
    let release_rx = Arc::new(Mutex::new(release_rx));
    let release_rx_for_builder = release_rx.clone();

    let builder = Arc::new(
        move |_bind_ctx: BindCtx,
              _port: u16,
              _name: String,
              _stop_flag: Arc<AtomicBool>,
              _channel: Arc<dyn ChannelLike>|
              -> Result<ReceiverBundle, BundleError> {
            let _ = release_rx_for_builder
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5));
            Ok(fake_bundle())
        },
    );

    let hook = make_stream_rebuild_hook(
        builder,
        bridge_cache,
        bridge_session.clone(),
        old_stop_flag.clone(),
        1,
        Some(instant_probe_fn()),
    );

    let (signal_tx, signal_rx) = sync_channel::<SupervisorSignal>(4);

    // Spawn hook — Gate A passes, teardown no-op, Gate B passes (stop_flag false),
    // builder starts and blocks on release_rx.
    let hook_handle = thread::Builder::new()
        .name("test-stream-hook-gate-c".into())
        .spawn(move || hook(signal_tx))
        .unwrap();

    // Give worker time to reach the builder block.
    thread::sleep(Duration::from_millis(20));

    // Set stop_flag while builder is blocked.
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
/// Controlled-timing technique (same as sender Gate D):
/// 1. Builder signals "started" via a channel then blocks on release_rx.
/// 2. Test waits for that signal, acquires bridge_session.lock() (step 6 already
///    released the lock — stream's step 6 takes the session then immediately
///    releases the lock before teardown).
/// 3. Test releases the builder, then sets stop_flag=true.
/// 4. Worker: Gate C passes (stop_flag was false), build_stream_session runs,
///    then tries bridge_session.lock() at Step 11 → BLOCKS (test holds it).
/// 5. Test releases the lock — worker swaps, hits Gate D, tears down, RebuildFailed.
///
/// This sequence is deterministic: the test holds the lock from before the builder
/// returns until after stop_flag is set, so Gate C always passes and Gate D always
/// fires (once implemented).
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

    let builder = Arc::new(
        move |_bind_ctx: BindCtx,
              _port: u16,
              _name: String,
              _stop_flag: Arc<AtomicBool>,
              _channel: Arc<dyn ChannelLike>|
              -> Result<ReceiverBundle, BundleError> {
            // Notify test that builder has started and is about to block.
            let _ = builder_started_tx.send(());
            // Block until released.
            let _ = release_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5));
            Ok(fake_bundle())
        },
    );

    let hook = make_stream_rebuild_hook(
        builder,
        bridge_cache,
        bridge_session.clone(),
        old_stop_flag.clone(),
        1,
        Some(instant_probe_fn()),
    );

    let (signal_tx, signal_rx) = sync_channel::<SupervisorSignal>(4);

    // Spawn hook — worker will proceed through Gate A, step 6 (releases lock
    // immediately after take), Gate B (false), then enter the blocking builder.
    let hook_handle = thread::Builder::new()
        .name("test-stream-hook-gate-d".into())
        .spawn(move || hook(signal_tx))
        .unwrap();

    // Wait for builder to start (means step 6 + probe + Gate B are done;
    // bridge_session lock is now available).
    builder_started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("builder must start within 5s");

    // Acquire bridge_session lock — step 6 has released it; worker is in builder.
    let guard = bridge_session.lock().unwrap();

    // Release the builder so it returns Ok.
    // stop_flag is still FALSE → Gate C will pass.
    let _ = release_tx.send(());

    // Give the worker time to:
    //   - receive the release signal (instant)
    //   - pass Gate C check (instant, stop_flag=false)
    //   - call build_stream_session → spawn mux (fast, ~µs)
    //   - call bridge_session.lock().unwrap() → BLOCK (we hold guard)
    // 5ms is a generous margin for these sub-millisecond operations.
    thread::sleep(Duration::from_millis(5));

    // Set stop_flag=true while worker is blocked at step 11 (bridge_session held).
    old_stop_flag.store(true, Ordering::Relaxed);

    // Release lock — worker swaps, hits Gate D, tears down, signals RebuildFailed.
    drop(guard);

    let outcome = signal_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("expected a signal within 5s");

    hook_handle.join().expect("hook thread must not panic");

    assert!(
        matches!(outcome, SupervisorSignal::RebuildFailed),
        "Gate D must produce RebuildFailed, got: {outcome:?}"
    );

    // Bridge session must be empty after Gate D tears down the newly-installed session.
    assert!(
        bridge_session.lock().unwrap().is_none(),
        "Gate D must tear down the newly-installed session (bridge_session must be None)"
    );
}
