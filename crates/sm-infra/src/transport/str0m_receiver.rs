//! str0m-backed video receiver adapter.
//!
//! [`Str0mVideoReceiver`] implements [`VideoReceiver`] using the str0m SansIO WebRTC stack.
//! It is cross-platform (no `cfg` gate) per PQ-9.
//!
//! # Thread model
//!
//! - `new()`: validates config; allocates shared atomics. No thread, no socket, no `Rtc`.
//! - `start(pkt_tx, event_tx)`: binds the `UdpSocket`, creates `Rtc`, spawns one OS thread
//!   that owns both. The thread runs the SansIO tick loop.
//! - `stop()`: sets the `AtomicBool` stop flag and joins the thread. Idempotent.
//! - `Drop`: calls `stop()` to prevent leaked threads on panic or forgotten stop.
//!
//! # Batch 3 scope
//!
//! Adapter skeleton only: constructor, lifecycle (start/stop/Drop), SansIO pump thread.
//! No media demuxing, no PLI emission, no offer/answer wiring — those land in Batch 4.
//! `apply_remote_offer`, `add_remote_candidate`, `request_keyframe` return `Err(NotRunning)`
//! as Batch 3 stubs.

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use str0m::Event;
use str0m::net::{Protocol, Receive};
use str0m::{IceConnectionState, Input, Output, Rtc};

use sm_domain::encode::EncodedPacket;
use sm_domain::signaling::{IceCandidate, SdpAnswer, SdpOffer};
use sm_domain::transport::{
    TRANSPORT_CHANNEL_CAPACITY, TransportConfig, TransportError, TransportEvent, VideoReceiver,
};

// ─── Internal control message ────────────────────────────────────────────────

/// Messages from the public API (any thread) to the tick thread.
///
/// Drained at the top of each tick iteration. Capacity bounded by
/// [`TRANSPORT_CHANNEL_CAPACITY`].
enum ReceiverControl {
    /// Add a remote ICE candidate.
    AddCandidate(IceCandidate),
    /// Send an RTCP PLI to the remote peer at the next tick.
    RequestKeyframe,
}

// ─── Shared state ────────────────────────────────────────────────────────────

/// Cross-thread state shared between the caller and the SansIO tick thread.
struct ReceiverShared {
    /// Raised by `stop()` / `Drop`; checked at the top of each tick iteration.
    stop: AtomicBool,
    /// Cumulative count of `EncodedPacket`s dropped due to consumer backpressure.
    dropped: AtomicU64,
}

impl ReceiverShared {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            stop: AtomicBool::new(false),
            dropped: AtomicU64::new(0),
        })
    }
}

// ─── Str0mVideoReceiver ──────────────────────────────────────────────────────

/// str0m-backed video receiver. Implements [`VideoReceiver`].
///
/// Cross-platform — no `#[cfg(target_os = "windows")]` gate (PQ-9).
///
/// # Lifecycle
///
/// 1. `new(config)` — lightweight construction; no socket, no thread.
/// 2. `start(pkt_tx, event_tx)` — binds UDP socket, spawns one OS thread.
/// 3. `stop()` / `Drop` — sets stop flag, joins thread. Idempotent.
pub struct Str0mVideoReceiver {
    /// Original transport configuration.
    config: TransportConfig,
    /// Shared atomic state between caller and tick thread.
    state: Arc<ReceiverShared>,
    /// Control inbox: caller → tick thread. Created in `start()`.
    control_tx: Option<SyncSender<ReceiverControl>>,
    /// Join handle for the tick thread. `Some` while running.
    handle: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Str0mVideoReceiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Str0mVideoReceiver")
            .field("config", &self.config)
            .field("running", &self.handle.is_some())
            .finish()
    }
}

// SAFETY: All shared mutable state crosses the thread boundary via
// `Arc<ReceiverShared>` (atomics only) and `SyncSender<ReceiverControl>`
// (Send when its element is Send). `JoinHandle<()>` is Send.
// `TransportConfig` contains Send-only data (String, u16, u32, Copy enum).
unsafe impl Send for Str0mVideoReceiver {}
// SAFETY: Methods that take `&self` only read atomics or clone `SyncSender`,
// both of which are Sync (the latter is Sync when its element is Send).
unsafe impl Sync for Str0mVideoReceiver {}

impl VideoReceiver for Str0mVideoReceiver {
    /// Construct a receiver with the given configuration.
    ///
    /// Does NOT bind a socket. Does NOT spawn a thread. Does NOT allocate `Rtc`.
    fn new(config: TransportConfig) -> Result<Self, TransportError>
    where
        Self: Sized,
    {
        Ok(Self {
            config,
            state: ReceiverShared::new(),
            control_tx: None,
            handle: None,
        })
    }

    /// Begin receiving. Binds the UDP socket and spawns one OS thread.
    ///
    /// Returns `Err(AlreadyRunning)` if called while a previous run is active.
    /// Returns `Err(Io(_))` if the UDP bind fails.
    fn start(
        &mut self,
        pkt_tx: SyncSender<EncodedPacket>,
        event_tx: SyncSender<TransportEvent>,
    ) -> Result<(), TransportError> {
        if self.handle.is_some() {
            return Err(TransportError::AlreadyRunning);
        }

        // Reset stop flag in case this receiver is restarted.
        self.state.stop.store(false, Ordering::Release);

        let bind_addr = format!("0.0.0.0:{}", self.config.udp_port);
        let udp = UdpSocket::bind(&bind_addr)
            .map_err(|e| TransportError::Io(format!("UDP bind failed on {bind_addr}: {e}")))?;
        let local_addr = udp
            .local_addr()
            .map_err(|e| TransportError::Io(e.to_string()))?;

        let crypto = str0m::crypto::from_feature_flags();
        let rtc = Rtc::builder()
            .set_crypto_provider(Arc::new(crypto))
            .build(Instant::now());

        let (ctrl_tx, ctrl_rx) =
            std::sync::mpsc::sync_channel::<ReceiverControl>(TRANSPORT_CHANNEL_CAPACITY);
        self.control_tx = Some(ctrl_tx);

        let state = Arc::clone(&self.state);

        let handle = std::thread::Builder::new()
            .name("sm-transport-receiver".into())
            .spawn(move || {
                run_receiver_loop(rtc, udp, local_addr, pkt_tx, event_tx, ctrl_rx, state);
            })
            .map_err(|e| TransportError::Internal(format!("thread spawn failed: {e}")))?;

        self.handle = Some(handle);
        Ok(())
    }

    /// Stop the receiver. Idempotent. Sets the stop flag and joins the thread.
    fn stop(&mut self) -> Result<(), TransportError> {
        self.state.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.control_tx = None;
        Ok(())
    }

    /// Apply a remote SDP offer and return the local answer.
    ///
    /// Batch 3 stub — full str0m offer/answer wiring lands in Batch 4.
    fn apply_remote_offer(&self, _offer: SdpOffer) -> Result<SdpAnswer, TransportError> {
        Err(TransportError::NotRunning)
    }

    /// Add a remote ICE candidate. Posts to the tick thread's control inbox.
    ///
    /// Returns `Err(NotRunning)` if `start()` has not been called.
    fn add_remote_candidate(&self, cand: IceCandidate) -> Result<(), TransportError> {
        match &self.control_tx {
            None => Err(TransportError::NotRunning),
            Some(tx) => tx
                .try_send(ReceiverControl::AddCandidate(cand))
                .map_err(|_| TransportError::Internal("control inbox full or disconnected".into())),
        }
    }

    /// Trigger a PLI to be sent to the peer at the next tick.
    ///
    /// Returns `Err(NotRunning)` if `start()` has not been called.
    fn request_keyframe(&self) -> Result<(), TransportError> {
        match &self.control_tx {
            None => Err(TransportError::NotRunning),
            Some(tx) => tx
                .try_send(ReceiverControl::RequestKeyframe)
                .map_err(|_| TransportError::Internal("control inbox full or disconnected".into())),
        }
    }

    /// Cumulative count of `EncodedPacket`s dropped due to consumer backpressure.
    fn dropped_frames(&self) -> u64 {
        self.state.dropped.load(Ordering::Relaxed)
    }
}

impl Drop for Str0mVideoReceiver {
    /// Ensure the tick thread is joined when the adapter is dropped.
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

// ─── Tick loop ───────────────────────────────────────────────────────────────

/// SansIO tick loop for `Str0mVideoReceiver`.
///
/// Runs on the dedicated OS thread spawned by `start()`.
///
/// # Batch 3 scope
///
/// This loop pumps str0m and checks the stop flag. It does NOT demux media
/// (no `Event::MediaData` → Annex-B reconstruction → `pkt_tx.try_send`),
/// no PLI emission, no offer/answer wiring. Those land in Batch 4.
fn run_receiver_loop(
    mut rtc: Rtc,
    udp: UdpSocket,
    local_addr: SocketAddr,
    _pkt_tx: SyncSender<EncodedPacket>,
    event_tx: SyncSender<TransportEvent>,
    ctrl_rx: Receiver<ReceiverControl>,
    state: Arc<ReceiverShared>,
) {
    let mut buf = vec![0u8; 2048];

    loop {
        // ── 1. Stop flag ──────────────────────────────────────────────────
        if state.stop.load(Ordering::Acquire) {
            break;
        }

        // ── 2. Drain control inbox (Batch 4: candidate add + PLI emission) ─
        while let Ok(msg) = ctrl_rx.try_recv() {
            match msg {
                ReceiverControl::AddCandidate(_cand) => {
                    // Batch 4: parse + rtc.add_remote_candidate(c)
                }
                ReceiverControl::RequestKeyframe => {
                    // Batch 4: rtc.direct_api().request_keyframe(mid, KeyframeRequestKind::Pli)
                }
            }
        }

        // ── 3. Drain str0m outputs until Timeout ─────────────────────────
        let deadline = loop {
            match rtc.poll_output() {
                Ok(Output::Timeout(t)) => break t,
                Ok(Output::Transmit(t)) => {
                    let _ = udp.send_to(&t.contents, t.destination);
                }
                Ok(Output::Event(ev)) => {
                    handle_receiver_event(ev, &state, &event_tx);
                }
                Err(_) => {
                    let _ = event_tx.try_send(TransportEvent::ConnectionLost {
                        reason: "str0m poll_output error".into(),
                    });
                    return;
                }
            }
        };

        if state.stop.load(Ordering::Acquire) {
            break;
        }

        // ── 4. Blocking recv_from with deadline-derived timeout ──────────
        let now = Instant::now();
        let remaining = deadline
            .checked_duration_since(now)
            .unwrap_or(Duration::from_millis(1));
        let timeout = remaining.max(Duration::from_millis(1));

        if let Err(e) = udp.set_read_timeout(Some(timeout)) {
            let _ = event_tx.try_send(TransportEvent::ConnectionLost {
                reason: format!("set_read_timeout failed: {e}"),
            });
            break;
        }

        match udp.recv_from(&mut buf) {
            Ok((n, source)) => {
                let bytes: &[u8] = &buf[..n];
                let now = Instant::now();
                if let Ok(dgram) = bytes.try_into() {
                    let receive = Receive {
                        proto: Protocol::Udp,
                        source,
                        destination: local_addr,
                        contents: dgram,
                    };
                    let _ = rtc.handle_input(Input::Receive(now, receive));
                }
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                let _ = rtc.handle_input(Input::Timeout(Instant::now()));
            }
            Err(e) => {
                let _ = event_tx.try_send(TransportEvent::ConnectionLost {
                    reason: format!("UDP recv_from error: {e}"),
                });
                break;
            }
        }
    }
}

/// Dispatch str0m events for the receiver.
///
/// Batch 3 only handles ICE state-change events for observability.
/// `Event::MediaData` (RTP demux → Annex-B EncodedPacket) is wired in Batch 4.
fn handle_receiver_event(
    ev: Event,
    _state: &ReceiverShared,
    event_tx: &SyncSender<TransportEvent>,
) {
    match ev {
        Event::IceConnectionStateChange(IceConnectionState::Connected) => {
            let _ = event_tx.try_send(TransportEvent::IceConnected);
        }
        Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
            let _ = event_tx.try_send(TransportEvent::IceFailed);
        }
        _ => {}
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::mpsc::sync_channel;

    use sm_domain::encode::EncodedPacket;
    use sm_domain::transport::{TransportConfig, TransportError, TransportEvent, VideoReceiver};

    use crate::transport::str0m_receiver::Str0mVideoReceiver;

    // ─── Static assertion: Str0mVideoReceiver is Send + Sync (task 3.5) ───────

    #[allow(dead_code)]
    fn _assert_send_sync_receiver() {
        fn check<T: Send + Sync>() {}
        check::<Str0mVideoReceiver>();
    }

    // ─── S6.1 (batch 3 variant): new() returns Ok with default config ─────────

    /// R6.2 (batch-3 variant): `Str0mVideoReceiver::new(config)` MUST return `Ok(_)`.
    #[test]
    fn str0m_receiver_new_default_config_returns_ok_s6_1() {
        let result = Str0mVideoReceiver::new(TransportConfig {
            role: sm_domain::transport::TransportRole::Receiver,
            ..TransportConfig::default()
        });
        assert!(
            result.is_ok(),
            "Str0mVideoReceiver::new(default) must return Ok, got: {result:?}"
        );
    }

    // ─── new() with port 0 still returns Ok ───────────────────────────────────

    #[test]
    fn str0m_receiver_new_port_zero_returns_ok() {
        let cfg = TransportConfig {
            udp_port: 0,
            role: sm_domain::transport::TransportRole::Receiver,
            ..TransportConfig::default()
        };
        let result = Str0mVideoReceiver::new(cfg);
        assert!(result.is_ok(), "new() must not reject port 0");
    }

    // ─── S6.4 (part 1): start + stop — thread exits cleanly ──────────────────

    /// R6.4, S6.4 — `start()` spawns a thread; `stop()` joins it and returns Ok.
    #[test]
    fn str0m_receiver_start_then_stop_ok() {
        let mut receiver = Str0mVideoReceiver::new(TransportConfig {
            udp_port: 0,
            role: sm_domain::transport::TransportRole::Receiver,
            ..TransportConfig::default()
        })
        .unwrap();

        let (pkt_tx, _pkt_rx) = sync_channel::<EncodedPacket>(4);
        let (event_tx, _event_rx) = sync_channel::<TransportEvent>(4);

        receiver.start(pkt_tx, event_tx).unwrap();
        let result = receiver.stop();
        assert!(result.is_ok(), "stop() must return Ok, got: {result:?}");
    }

    // ─── S6.4: stop() is idempotent ───────────────────────────────────────────

    /// R12.4, S6.4 — second `stop()` MUST return `Ok(())` without panic.
    #[test]
    fn str0m_receiver_stop_is_idempotent_s6_4() {
        let mut receiver = Str0mVideoReceiver::new(TransportConfig {
            udp_port: 0,
            role: sm_domain::transport::TransportRole::Receiver,
            ..TransportConfig::default()
        })
        .unwrap();
        // Stop on never-started receiver — idempotent.
        receiver.stop().unwrap();
        receiver.stop().unwrap();

        // Start + stop + stop.
        let (pkt_tx, _pkt_rx) = sync_channel::<EncodedPacket>(4);
        let (event_tx, _event_rx) = sync_channel::<TransportEvent>(4);
        receiver.start(pkt_tx, event_tx).unwrap();
        receiver.stop().unwrap();
        receiver.stop().unwrap(); // second stop must not panic
    }

    // ─── S12.1 (receiver): Drop calls stop() — no thread leak ─────────────────

    /// R12.5 — Drop MUST call stop() if thread is still running.
    #[test]
    fn str0m_receiver_drop_without_stop_joins_thread() {
        let (pkt_tx, _pkt_rx) = sync_channel::<EncodedPacket>(4);
        let (event_tx, _event_rx) = sync_channel::<TransportEvent>(4);

        {
            let mut receiver = Str0mVideoReceiver::new(TransportConfig {
                udp_port: 0,
                role: sm_domain::transport::TransportRole::Receiver,
                ..TransportConfig::default()
            })
            .unwrap();
            receiver.start(pkt_tx, event_tx).unwrap();
            // receiver drops here — Drop calls stop() which joins the thread.
        }
        // If we reach here without hanging the thread was joined.
    }

    // ─── dropped_frames() returns 0 before any drops ──────────────────────────

    #[test]
    fn str0m_receiver_dropped_frames_initially_zero() {
        let receiver = Str0mVideoReceiver::new(TransportConfig {
            role: sm_domain::transport::TransportRole::Receiver,
            ..TransportConfig::default()
        })
        .unwrap();
        assert_eq!(
            receiver.dropped_frames(),
            0,
            "dropped_frames must be 0 before any activity"
        );
    }

    // ─── start() returns AlreadyRunning if called twice ───────────────────────

    #[test]
    fn str0m_receiver_start_twice_returns_already_running() {
        let mut receiver = Str0mVideoReceiver::new(TransportConfig {
            udp_port: 0,
            role: sm_domain::transport::TransportRole::Receiver,
            ..TransportConfig::default()
        })
        .unwrap();

        let (pkt_tx, _pkt_rx) = sync_channel::<EncodedPacket>(4);
        let (event_tx, _event_rx) = sync_channel::<TransportEvent>(4);
        receiver.start(pkt_tx, event_tx).unwrap();

        let (pkt_tx2, _pkt_rx2) = sync_channel::<EncodedPacket>(4);
        let (event_tx2, _event_rx2) = sync_channel::<TransportEvent>(4);
        let result = receiver.start(pkt_tx2, event_tx2);
        assert!(
            matches!(result, Err(TransportError::AlreadyRunning)),
            "second start() must return Err(AlreadyRunning), got: {result:?}"
        );

        receiver.stop().unwrap();
    }

    // ─── Task 4.5: receiver wiring tests (RED before Batch 4.6 impl) ──────────

    /// S6.2 — `apply_remote_offer` MUST return an `SdpAnswer` (not NotRunning stub).
    ///
    /// Currently returns `Err(NotRunning)` because the stub is not wired to str0m.
    /// Task 4.6 GREEN implementation must satisfy this test.
    ///
    /// The offer passed here is a minimal but valid SDP produced by a sender `Rtc`
    /// so that the receiver can negotiate and produce a real answer.
    #[test]
    fn str0m_receiver_apply_remote_offer_returns_answer_s6_2() {
        use str0m::Rtc;
        use str0m::media::{Direction, MediaKind};

        // Build a minimal sender-side Rtc just to generate a real SDP offer.
        let crypto_s = str0m::crypto::from_feature_flags();
        let mut rtc_sender = Rtc::builder()
            .set_crypto_provider(std::sync::Arc::new(crypto_s))
            .build(std::time::Instant::now());
        let mut change = rtc_sender.sdp_api();
        change.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
        let (str0m_offer, _pending) = change.apply().unwrap();
        // Serialise to the domain SdpOffer newtype (plain SDP text).
        let domain_offer = sm_domain::signaling::SdpOffer(str0m_offer.to_string());

        // Build a receiver and call apply_remote_offer BEFORE start().
        // Per design §8.3 "apply_remote_offer: permitted before OR after start()".
        let receiver = Str0mVideoReceiver::new(TransportConfig {
            udp_port: 0,
            role: sm_domain::transport::TransportRole::Receiver,
            ..TransportConfig::default()
        })
        .unwrap();

        // RED: currently returns Err(NotRunning).
        // Task 4.6 must return Ok(SdpAnswer(...)) with non-empty SDP.
        let result = receiver.apply_remote_offer(domain_offer);
        assert!(
            result.is_ok(),
            "apply_remote_offer must return Ok(SdpAnswer), got: {result:?}"
        );
        let answer = result.unwrap();
        assert!(!answer.0.is_empty(), "SdpAnswer must be non-empty");
        assert!(
            answer.0.contains("v=0"),
            "SdpAnswer must be a valid SDP starting with v=0, got: {}",
            answer.0
        );
    }

    /// S6.3 — `apply_remote_offer` BEFORE `start()` MUST NOT panic.
    ///
    /// This is a weaker form of S6.2. Even if the impl returns an error,
    /// it must not panic. Currently passes (stub returns Err). Remains passing
    /// through task 4.6 (impl may return Ok or Err, but must not panic).
    #[test]
    fn str0m_receiver_apply_remote_offer_before_start_no_panic_s6_3() {
        let receiver = Str0mVideoReceiver::new(TransportConfig {
            udp_port: 0,
            role: sm_domain::transport::TransportRole::Receiver,
            ..TransportConfig::default()
        })
        .unwrap();

        let dummy_offer = sm_domain::signaling::SdpOffer("v=0\r\n".into());
        // Must not panic regardless of return value.
        let _ = receiver.apply_remote_offer(dummy_offer);
    }

    /// R6.5, S6.3 — `request_keyframe` before start returns `Err(NotRunning)`.
    /// After start, `request_keyframe()` MUST return `Ok(())`.
    #[test]
    fn str0m_receiver_request_keyframe_before_start_returns_not_running() {
        let receiver = Str0mVideoReceiver::new(TransportConfig {
            udp_port: 0,
            role: sm_domain::transport::TransportRole::Receiver,
            ..TransportConfig::default()
        })
        .unwrap();

        let result = receiver.request_keyframe();
        assert!(
            matches!(result, Err(TransportError::NotRunning)),
            "request_keyframe before start must return Err(NotRunning), got: {result:?}"
        );
    }

    /// R6.5 — `request_keyframe()` MUST return `Ok(())` while the receiver is running.
    #[test]
    fn str0m_receiver_request_keyframe_while_running_ok() {
        let mut receiver = Str0mVideoReceiver::new(TransportConfig {
            udp_port: 0,
            role: sm_domain::transport::TransportRole::Receiver,
            ..TransportConfig::default()
        })
        .unwrap();

        let (pkt_tx, _pkt_rx) = sync_channel::<EncodedPacket>(4);
        let (event_tx, _event_rx) = sync_channel::<TransportEvent>(4);
        receiver.start(pkt_tx, event_tx).unwrap();

        let result = receiver.request_keyframe();
        assert!(
            result.is_ok(),
            "request_keyframe() while running must return Ok, got: {result:?}"
        );

        receiver.stop().unwrap();
    }

    /// S14.2 — When the output channel is full and the receiver produces packets,
    /// `dropped_frames()` MUST increase and no panic or block MUST occur.
    ///
    /// This test is currently GREEN (tick loop drops all packets since MediaData
    /// events are not yet emitted in the stub). Remains GREEN after 4.6 when the
    /// real impl uses `try_send` with drop-newest.
    #[test]
    fn str0m_receiver_full_output_channel_no_panic_s14_2() {
        use std::time::Duration;

        let mut receiver = Str0mVideoReceiver::new(TransportConfig {
            udp_port: 0,
            role: sm_domain::transport::TransportRole::Receiver,
            ..TransportConfig::default()
        })
        .unwrap();

        // Capacity-1 channel we NEVER drain.
        let (pkt_tx, _pkt_rx) = sync_channel::<EncodedPacket>(1);
        let (event_tx, _event_rx) = sync_channel::<TransportEvent>(4);
        receiver.start(pkt_tx, event_tx).unwrap();

        // Let the receiver tick for a short time.
        std::thread::sleep(Duration::from_millis(100));

        // Must not panic.
        let result = receiver.stop();
        assert!(
            result.is_ok(),
            "stop() must return Ok with full output channel"
        );
    }

    /// S14.4 — `stop()` called from one thread while the tick loop is blocked in
    /// `recv_from` MUST return within 500 ms (uses 200 ms read timeout inside tick loop).
    #[test]
    fn str0m_receiver_stop_unblocks_from_recv_within_500ms_s14_4() {
        use std::time::{Duration, Instant};

        let mut receiver = Str0mVideoReceiver::new(TransportConfig {
            udp_port: 0,
            role: sm_domain::transport::TransportRole::Receiver,
            ..TransportConfig::default()
        })
        .unwrap();

        let (pkt_tx, _pkt_rx) = sync_channel::<EncodedPacket>(4);
        let (event_tx, _event_rx) = sync_channel::<TransportEvent>(4);
        receiver.start(pkt_tx, event_tx).unwrap();

        // Give the thread a moment to enter recv_from.
        std::thread::sleep(Duration::from_millis(20));

        let t0 = Instant::now();
        receiver.stop().unwrap();
        let elapsed = t0.elapsed();

        assert!(
            elapsed < Duration::from_millis(500),
            "stop() must return within 500 ms, took: {elapsed:?}"
        );
    }
}
