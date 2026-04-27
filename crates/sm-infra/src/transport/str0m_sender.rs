//! str0m-backed video sender adapter.
//!
//! [`Str0mVideoSender`] implements [`VideoSender`] using the str0m SansIO WebRTC stack.
//! It is cross-platform (no `cfg` gate) per PQ-9.
//!
//! # Thread model
//!
//! - `new()`: validates config; allocates shared atomics. No thread, no socket, no `Rtc`.
//! - `set_encoder()`: stores the encoder `Arc` for later PLI wiring (Batch 4).
//! - `start(rx, event_tx)`: binds the `UdpSocket`, creates `Rtc`, spawns one OS thread
//!   that owns both. The thread runs the SansIO tick loop.
//! - `stop()`: sets the `AtomicBool` stop flag and joins the thread. Idempotent.
//! - `Drop`: calls `stop()` to prevent leaked threads on panic or forgotten stop.
//!
//! # Batch 3 scope
//!
//! This batch lands the adapter skeleton: constructor, lifecycle (start/stop/Drop),
//! and the SansIO pump thread. No media writing, no PLI handling, no offer/answer
//! wiring — those are Batch 4.

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use str0m::Event;
use str0m::net::{Protocol, Receive};
use str0m::{IceConnectionState, Input, Output, Rtc};

use sm_domain::encode::{EncodedPacket, VideoEncoder};
use sm_domain::signaling::{IceCandidate, SdpAnswer, SdpOffer};
use sm_domain::transport::{
    TRANSPORT_CHANNEL_CAPACITY, TransportConfig, TransportError, TransportEvent, VideoSender,
};

// ─── Internal control message ────────────────────────────────────────────────

/// Messages sent from the public API (any thread) to the tick loop thread.
///
/// The tick loop drains this inbox at the start of each iteration.
/// Capacity is bounded (`TRANSPORT_CHANNEL_CAPACITY`).
enum SenderControl {
    /// Apply a remote SDP answer.
    ApplyAnswer(SdpAnswer),
    /// Add a remote ICE candidate.
    AddCandidate(IceCandidate),
}

// ─── Shared state ────────────────────────────────────────────────────────────

/// Cross-thread state shared between the caller and the SansIO tick thread.
///
/// All fields are atomics or guarded by `Mutex` so no lock contention in the hot path.
struct SenderShared {
    /// Raised by `stop()` / `Drop`; checked at the top of each tick iteration.
    stop: AtomicBool,
    /// Cumulative count of `EncodedPacket`s dropped due to send-side congestion.
    dropped: AtomicU64,
}

impl SenderShared {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            stop: AtomicBool::new(false),
            dropped: AtomicU64::new(0),
        })
    }
}

// ─── Str0mVideoSender ────────────────────────────────────────────────────────

/// str0m-backed video sender. Implements [`VideoSender`].
///
/// Cross-platform — no `#[cfg(target_os = "windows")]` gate (PQ-9).
///
/// # Lifecycle
///
/// 1. `new(config)` — lightweight construction; no socket, no thread.
/// 2. `set_encoder(arc)` — inject encoder for PLI feedback (wired in Batch 4).
/// 3. `start(rx, event_tx)` — binds UDP socket, spawns one OS thread.
/// 4. `stop()` / `Drop` — sets stop flag, joins thread. Idempotent.
pub struct Str0mVideoSender {
    /// Original transport configuration.
    config: TransportConfig,
    /// Shared atomic state between caller and tick thread.
    state: Arc<SenderShared>,
    /// Encoder held for PLI wiring (Batch 4). Unused in Batch 3.
    encoder: Option<Arc<dyn VideoEncoder + Send + Sync>>,
    /// Control inbox: caller → tick thread. Created in `start()`.
    control_tx: Option<SyncSender<SenderControl>>,
    /// Join handle for the tick thread. `Some` while running, `None` otherwise.
    handle: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Str0mVideoSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Str0mVideoSender")
            .field("config", &self.config)
            .field("running", &self.handle.is_some())
            .field("encoder_set", &self.encoder.is_some())
            .finish()
    }
}

// SAFETY: All shared mutable state goes via `Arc<SenderShared>` (atomics only)
// and `SyncSender<SenderControl>` which is `Send`. `JoinHandle<()>` is `Send`.
// The encoder is `Arc<dyn VideoEncoder + Send + Sync>` — both `Send` and `Sync`.
// `TransportConfig` contains only `Send` data (String, u16, u32, Copy enum).
unsafe impl Send for Str0mVideoSender {}
// SAFETY: Every method that takes `&self` only accesses atomics (which are `Sync`)
// or clones the `Arc` (also `Sync`). `SyncSender<T>: Sync` when `T: Send`.
unsafe impl Sync for Str0mVideoSender {}

impl VideoSender for Str0mVideoSender {
    /// Construct a sender with the given configuration.
    ///
    /// Does NOT bind a socket. Does NOT spawn a thread. Does NOT allocate `Rtc`.
    /// Returns `Ok(_)` for any valid `TransportConfig` value.
    fn new(config: TransportConfig) -> Result<Self, TransportError>
    where
        Self: Sized,
    {
        Ok(Self {
            config,
            state: SenderShared::new(),
            encoder: None,
            control_tx: None,
            handle: None,
        })
    }

    /// Inject the encoder reference for PLI feedback (used in Batch 4).
    ///
    /// MUST be called before [`start`](VideoSender::start). Stored as
    /// `Arc<dyn VideoEncoder + Send + Sync>` so the tick thread can call
    /// `request_keyframe()` directly on RTCP PLI events.
    fn set_encoder(&mut self, encoder: Arc<dyn VideoEncoder + Send + Sync>) {
        self.encoder = Some(encoder);
    }

    /// Begin sending. Binds the UDP socket and spawns one OS thread.
    ///
    /// Returns `Err(AlreadyRunning)` if called while the thread is active.
    /// Returns `Err(Io(_))` if the UDP bind fails.
    fn start(
        &mut self,
        rx: Receiver<EncodedPacket>,
        event_tx: SyncSender<TransportEvent>,
    ) -> Result<(), TransportError> {
        if self.handle.is_some() {
            return Err(TransportError::AlreadyRunning);
        }

        // Reset stop flag in case this sender is restarted.
        self.state.stop.store(false, Ordering::Release);

        // Bind the UDP socket here (not in new()) so two adapters constructed
        // at test time don't fight over the port until they are actually started.
        let bind_addr = format!("0.0.0.0:{}", self.config.udp_port);
        let udp = UdpSocket::bind(&bind_addr)
            .map_err(|e| TransportError::Io(format!("UDP bind failed on {bind_addr}: {e}")))?;

        let local_addr = udp
            .local_addr()
            .map_err(|e| TransportError::Io(e.to_string()))?;

        // Build the str0m Rtc instance with the rust-crypto backend.
        let crypto = str0m::crypto::from_feature_flags();
        let rtc = Rtc::builder()
            .set_crypto_provider(Arc::new(crypto))
            .build(Instant::now());

        // Control inbox: bounded channel for offer/answer/candidate messages.
        let (ctrl_tx, ctrl_rx) =
            std::sync::mpsc::sync_channel::<SenderControl>(TRANSPORT_CHANNEL_CAPACITY);
        self.control_tx = Some(ctrl_tx);

        let state = Arc::clone(&self.state);
        // NOTE: encoder is cloned into the thread for Batch 4 PLI wiring.
        // In Batch 3 it is not used inside the loop.
        let _encoder = self.encoder.clone();

        let handle = std::thread::Builder::new()
            .name("sm-transport-sender".into())
            .spawn(move || {
                run_sender_loop(rtc, udp, local_addr, rx, event_tx, ctrl_rx, state);
            })
            .map_err(|e| TransportError::Internal(format!("thread spawn failed: {e}")))?;

        self.handle = Some(handle);
        Ok(())
    }

    /// Stop the sender. Idempotent. Sets the stop flag and joins the thread.
    ///
    /// Returns `Ok(())` even if the sender was never started.
    fn stop(&mut self) -> Result<(), TransportError> {
        self.state.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.control_tx = None;
        Ok(())
    }

    /// Apply a remote SDP answer. Posts to the tick thread's control inbox.
    ///
    /// Returns `Err(NotRunning)` if `start()` has not been called.
    fn apply_remote_answer(&self, answer: SdpAnswer) -> Result<(), TransportError> {
        match &self.control_tx {
            None => Err(TransportError::NotRunning),
            Some(tx) => tx
                .try_send(SenderControl::ApplyAnswer(answer))
                .map_err(|_| TransportError::Internal("control inbox full or disconnected".into())),
        }
    }

    /// Add a remote ICE candidate. Posts to the tick thread's control inbox.
    ///
    /// Returns `Err(NotRunning)` if `start()` has not been called.
    fn add_remote_candidate(&self, cand: IceCandidate) -> Result<(), TransportError> {
        match &self.control_tx {
            None => Err(TransportError::NotRunning),
            Some(tx) => tx
                .try_send(SenderControl::AddCandidate(cand))
                .map_err(|_| TransportError::Internal("control inbox full or disconnected".into())),
        }
    }

    /// Produce the local SDP offer.
    ///
    /// In Batch 3 this is a stub — full implementation (str0m SDP offer generation)
    /// lands in Batch 4. Returns `Err(NotRunning)` always in Batch 3.
    fn create_local_offer(&self) -> Result<SdpOffer, TransportError> {
        // Batch 4: wire str0m SdpApi here.
        Err(TransportError::NotRunning)
    }

    /// Cumulative count of `EncodedPacket`s dropped due to send-side congestion.
    fn dropped_frames(&self) -> u64 {
        self.state.dropped.load(Ordering::Relaxed)
    }
}

impl Drop for Str0mVideoSender {
    /// Ensure the tick thread is joined when the adapter is dropped.
    ///
    /// Mirrors `WindowsOpenH264Encoder::Drop` (R12.5).
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

// ─── Tick loop ───────────────────────────────────────────────────────────────

/// SansIO tick loop for `Str0mVideoSender`.
///
/// Runs on the dedicated OS thread spawned by `start()`.
///
/// # Batch 3 scope
///
/// This loop pumps str0m and checks the stop flag. It does NOT process media
/// (no `rx.recv()` → `writer.write()`), no PLI handling, no offer/answer wiring.
/// Those are Batch 4. The loop is intentionally minimal so the lifecycle tests
/// (start/stop/Drop) pass without requiring a fully-wired str0m session.
fn run_sender_loop(
    mut rtc: Rtc,
    udp: UdpSocket,
    local_addr: SocketAddr,
    rx: Receiver<EncodedPacket>,
    event_tx: SyncSender<TransportEvent>,
    ctrl_rx: std::sync::mpsc::Receiver<SenderControl>,
    state: Arc<SenderShared>,
) {
    // Read buffer for incoming UDP datagrams.
    let mut buf = vec![0u8; 2048];

    loop {
        // ── 1. Stop flag ──────────────────────────────────────────────────
        if state.stop.load(Ordering::Acquire) {
            break;
        }

        // ── 2. Drain control inbox (Batch 4 will wire offer/answer here) ──
        while let Ok(msg) = ctrl_rx.try_recv() {
            match msg {
                SenderControl::ApplyAnswer(_answer) => {
                    // Batch 4: rtc.sdp_api().accept_answer(pending, answer_sdp)
                }
                SenderControl::AddCandidate(_cand) => {
                    // Batch 4: parse candidate string → Candidate, rtc.add_remote_candidate(c)
                }
            }
        }

        // ── 3. Drain encoded packets from channel (Batch 4 will write to str0m) ─
        while let Ok(_pkt) = rx.try_recv() {
            // Batch 4: iter_nal_units + writer.write(pt, wallclock, rtp_time, nal)
            // For now, silently drop packets to keep the thread from blocking.
        }

        // ── 4. Drain str0m outputs until Timeout ─────────────────────────
        let deadline = loop {
            match rtc.poll_output() {
                Ok(Output::Timeout(t)) => break t,
                Ok(Output::Transmit(t)) => {
                    let _ = udp.send_to(&t.contents, t.destination);
                }
                Ok(Output::Event(ev)) => {
                    handle_sender_event(ev, &state, &event_tx);
                }
                Err(_) => {
                    // str0m error — surface as ConnectionLost and exit.
                    let _ = event_tx.try_send(TransportEvent::ConnectionLost {
                        reason: "str0m poll_output error".into(),
                    });
                    return;
                }
            }
        };

        // Re-check stop flag before blocking on recv_from.
        if state.stop.load(Ordering::Acquire) {
            break;
        }

        // ── 5. Blocking recv_from with deadline-derived timeout ──────────
        let now = Instant::now();
        let remaining = deadline
            .checked_duration_since(now)
            .unwrap_or(Duration::from_millis(1));
        // Clamp: set_read_timeout(Some(0)) is not allowed.
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
                // Expected timeout — drive str0m time forward.
                let _ = rtc.handle_input(Input::Timeout(Instant::now()));
            }
            Err(e) => {
                // Unexpected socket error — surface and exit.
                let _ = event_tx.try_send(TransportEvent::ConnectionLost {
                    reason: format!("UDP recv_from error: {e}"),
                });
                break;
            }
        }
    }
    // Thread exits cleanly — socket dropped here.
}

/// Dispatch str0m events for the sender.
///
/// In Batch 3 only ICE state-change events are handled (observability).
/// PLI dispatch (`Event::KeyframeRequest`) is wired in Batch 4.
fn handle_sender_event(ev: Event, _state: &SenderShared, event_tx: &SyncSender<TransportEvent>) {
    match ev {
        Event::IceConnectionStateChange(IceConnectionState::Connected) => {
            let _ = event_tx.try_send(TransportEvent::IceConnected);
        }
        Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
            let _ = event_tx.try_send(TransportEvent::IceFailed);
        }
        Event::KeyframeRequest(_req) => {
            // Batch 4: call encoder.request_keyframe() + emit TransportEvent::KeyframeRequested
        }
        _ => {}
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc::sync_channel;

    use sm_domain::encode::{EncoderConfig, VideoEncoder};
    use sm_domain::transport::{TransportConfig, TransportError, TransportEvent, VideoSender};

    use crate::transport::str0m_sender::Str0mVideoSender;

    // ─── Static assertion: Str0mVideoSender is Send + Sync (task 3.5) ─────────
    //
    // Guards PQ-9 (cross-platform) + design §3.1 claim that the adapter is Send.
    // The Sync bound is needed because the trait is `Send` and callers may hold
    // `Arc<dyn VideoSender>` for stats polling from another thread.

    #[allow(dead_code)]
    fn _assert_send_sync_sender() {
        fn check<T: Send + Sync>() {}
        check::<Str0mVideoSender>();
    }

    // ─── Helper: minimal FakeVideoEncoder for encoder injection tests ─────────

    struct FakeEncoder {
        keyframe_called: Arc<AtomicBool>,
        dropped: Arc<AtomicU64>,
    }

    impl FakeEncoder {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                keyframe_called: Arc::new(AtomicBool::new(false)),
                dropped: Arc::new(AtomicU64::new(0)),
            })
        }
    }

    impl VideoEncoder for FakeEncoder {
        fn new(_config: EncoderConfig) -> Result<Self, sm_domain::encode::EncoderError>
        where
            Self: Sized,
        {
            Ok(Self {
                keyframe_called: Arc::new(AtomicBool::new(false)),
                dropped: Arc::new(AtomicU64::new(0)),
            })
        }

        fn start(
            &mut self,
            _rx: std::sync::mpsc::Receiver<sm_domain::CaptureFrame>,
            _tx: std::sync::mpsc::SyncSender<sm_domain::encode::EncodedPacket>,
        ) -> Result<(), sm_domain::encode::EncoderError> {
            Ok(())
        }

        fn stop(&mut self) -> Result<(), sm_domain::encode::EncoderError> {
            Ok(())
        }

        fn request_keyframe(&self) {
            self.keyframe_called.store(true, Ordering::Release);
        }

        fn set_bitrate(&self, _bps: u32) -> Result<(), sm_domain::encode::EncoderError> {
            Ok(())
        }

        fn dropped_frames(&self) -> u64 {
            self.dropped.load(Ordering::Relaxed)
        }
    }

    // ─── S5.1 (batch 3 variant): new() returns Ok with default config ─────────

    /// R5.2 (batch-3 variant): `Str0mVideoSender::new(config)` MUST return `Ok(_)`.
    #[test]
    fn str0m_sender_new_default_config_returns_ok_s5_1() {
        let result = Str0mVideoSender::new(TransportConfig::default());
        assert!(
            result.is_ok(),
            "Str0mVideoSender::new(default) must return Ok, got: {result:?}"
        );
    }

    // ─── new() with port 0 still returns Ok (validation deferred to start) ────

    #[test]
    fn str0m_sender_new_port_zero_returns_ok() {
        let cfg = TransportConfig {
            udp_port: 0,
            ..TransportConfig::default()
        };
        // Port 0 is valid — OS picks an ephemeral port on bind in start().
        let result = Str0mVideoSender::new(cfg);
        assert!(result.is_ok(), "new() must not reject port 0");
    }

    // ─── set_encoder stores the encoder (no panic) ───────────────────────────

    #[test]
    fn str0m_sender_set_encoder_no_panic() {
        let mut sender = Str0mVideoSender::new(TransportConfig::default()).unwrap();
        let enc = FakeEncoder::new();
        // Must not panic.
        sender.set_encoder(enc as Arc<dyn VideoEncoder + Send + Sync>);
    }

    // ─── S5.2: start + stop — thread exits cleanly ───────────────────────────

    /// R5.3, S5.2 — `start()` spawns a thread; `stop()` joins it and returns Ok.
    /// Uses port 0 so the OS picks a free ephemeral port — no conflict on parallel test runs.
    #[test]
    fn str0m_sender_start_then_stop_ok_s5_2() {
        let mut sender = Str0mVideoSender::new(TransportConfig {
            udp_port: 0,
            ..TransportConfig::default()
        })
        .unwrap();
        let enc = FakeEncoder::new();
        sender.set_encoder(enc as Arc<dyn VideoEncoder + Send + Sync>);

        let (pkt_tx, pkt_rx) = sync_channel(4);
        let (event_tx, _event_rx) = sync_channel::<TransportEvent>(4);

        sender.start(pkt_rx, event_tx).unwrap();
        drop(pkt_tx); // unblock thread's try_recv loop so stop() joins cleanly

        let result = sender.stop();
        assert!(result.is_ok(), "stop() must return Ok, got: {result:?}");
    }

    // ─── start + stop with pkt_tx dropped first ──────────────────────────────

    #[test]
    fn str0m_sender_stop_after_pkt_tx_dropped() {
        let mut sender = Str0mVideoSender::new(TransportConfig {
            udp_port: 0,
            ..TransportConfig::default()
        })
        .unwrap();
        let enc = FakeEncoder::new();
        sender.set_encoder(enc as Arc<dyn VideoEncoder + Send + Sync>);

        let (pkt_tx, pkt_rx) = sync_channel(4);
        let (event_tx, _event_rx) = sync_channel::<TransportEvent>(4);

        sender.start(pkt_rx, event_tx).unwrap();
        drop(pkt_tx);

        let result = sender.stop();
        assert!(result.is_ok(), "stop() must return Ok, got: {result:?}");
    }

    // ─── S12.4: stop() is idempotent ──────────────────────────────────────────

    /// R12.4, S12.4 — second `stop()` MUST return `Ok(())` without panic.
    #[test]
    fn str0m_sender_stop_is_idempotent_s12_4() {
        let mut sender = Str0mVideoSender::new(TransportConfig::default()).unwrap();
        // Stop on never-started sender — idempotent.
        sender.stop().unwrap();
        sender.stop().unwrap();

        // Start + stop + stop.
        let enc = FakeEncoder::new();
        sender.set_encoder(enc as Arc<dyn VideoEncoder + Send + Sync>);
        let (pkt_tx, pkt_rx) = sync_channel(4);
        let (event_tx, _event_rx) = sync_channel::<TransportEvent>(4);
        sender
            .start(pkt_rx, event_tx)
            .expect("start must succeed on port 7889 or fallback");
        drop(pkt_tx);
        sender.stop().unwrap();
        sender.stop().unwrap(); // second stop must not panic
    }

    // ─── S12.1: Drop calls stop() — no thread leak ────────────────────────────

    /// R12.5, S12.1 — Drop MUST call stop() if thread is still running.
    #[test]
    fn str0m_sender_drop_without_stop_joins_thread_s12_1() {
        let (pkt_tx, pkt_rx) = sync_channel(4);
        let (event_tx, _event_rx) = sync_channel::<TransportEvent>(4);

        {
            let mut sender = Str0mVideoSender::new(TransportConfig {
                udp_port: 0,
                ..TransportConfig::default()
            })
            .unwrap();
            let enc = FakeEncoder::new();
            sender.set_encoder(enc as Arc<dyn VideoEncoder + Send + Sync>);
            sender.start(pkt_rx, event_tx).unwrap();
            drop(pkt_tx); // ensure thread can exit
            // sender drops here — Drop calls stop() which joins the thread.
        }
        // If we reach here without hanging the thread was joined.
    }

    // ─── dropped_frames() returns 0 before any drops ──────────────────────────

    #[test]
    fn str0m_sender_dropped_frames_initially_zero() {
        let sender = Str0mVideoSender::new(TransportConfig::default()).unwrap();
        assert_eq!(
            sender.dropped_frames(),
            0,
            "dropped_frames must be 0 before any activity"
        );
    }

    // ─── start() returns AlreadyRunning if called twice ───────────────────────

    #[test]
    fn str0m_sender_start_twice_returns_already_running() {
        let mut sender = Str0mVideoSender::new(TransportConfig {
            udp_port: 0,
            ..TransportConfig::default()
        })
        .unwrap();
        let enc = FakeEncoder::new();
        sender.set_encoder(enc as Arc<dyn VideoEncoder + Send + Sync>);

        let (pkt_tx, pkt_rx) = sync_channel(4);
        let (event_tx, _event_rx) = sync_channel::<TransportEvent>(4);
        sender.start(pkt_rx, event_tx).unwrap();

        let (_pkt_tx2, pkt_rx2) = sync_channel(4);
        let (event_tx2, _event_rx2) = sync_channel::<TransportEvent>(4);
        let result = sender.start(pkt_rx2, event_tx2);
        assert!(
            matches!(result, Err(TransportError::AlreadyRunning)),
            "second start() must return Err(AlreadyRunning), got: {result:?}"
        );

        drop(pkt_tx);
        sender.stop().unwrap();
    }
}
