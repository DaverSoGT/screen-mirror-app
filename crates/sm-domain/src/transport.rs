//! Port boundary for media transport.
//!
//! This module defines the domain-level contract for the WebRTC transport link.
//! No platform type, async runtime, str0m, or codec-specific import is permitted
//! here — all platform adaptation lives in `sm-infra`.
//!
//! # Key types
//!
//! | Type | Role |
//! |------|------|
//! | [`VideoSender`] | Port trait for the sending side (consumes `EncodedPacket`s, emits RTP). |
//! | [`VideoReceiver`] | Port trait for the receiving side (receives RTP, emits `EncodedPacket`s). |
//! | [`TransportConfig`] | Configuration: UDP port, H.264 profile, bitrate. |
//! | [`TransportEvent`] | Events emitted by transport adapters (ICE state, PLI, drops). |
//! | [`TransportError`] | Unified error enum for all transport operations. |
//! | [`TransportRole`] | Whether this instance is Sender or Receiver. |
//! | [`TRANSPORT_CHANNEL_CAPACITY`] | Bounded channel capacity constant (4). |

use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender};

use crate::encode::{EncodedPacket, VideoEncoder};
use crate::signaling::{IceCandidate, SdpAnswer, SdpOffer};

/// Bounded channel capacity for transport event/packet output.
///
/// Mirrors `CAPTURE_CHANNEL_CAPACITY` (4) and `ENCODE_CHANNEL_CAPACITY` (4)
/// to keep backpressure budget consistent across the pipeline.
/// At 60 fps a 4-slot bounded channel buffers ~67 ms.
pub const TRANSPORT_CHANNEL_CAPACITY: usize = 4;

/// Role of a transport instance.
///
/// Sender pushes `EncodedPacket`s out; receiver pulls them in.
/// One process = one role per session in V1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportRole {
    /// This instance encodes and transmits media.
    Sender,
    /// This instance receives and decodes media.
    Receiver,
}

/// Configuration for a transport session.
///
/// # Defaults
///
/// - `udp_port`: 7889 (PQ-1)
/// - `h264_profile`: `"640032"` (PQ-3 — High profile, level 5.0, compatible, CBR)
/// - `bitrate_bps`: 4 000 000 (matches `EncoderConfig::bitrate_bps` default)
/// - `role`: `TransportRole::Sender`
#[derive(Debug, Clone)]
pub struct TransportConfig {
    /// UDP port for RTP/SRTP and ICE host candidate.
    /// Default: 7889 (PQ-1).
    pub udp_port: u16,
    /// H.264 profile-level-id hex string used in SDP negotiation.
    /// Default: `"640032"` (High profile, level 5.0, CBR — PQ-3).
    pub h264_profile: String,
    /// Target bitrate in bits per second.
    /// Mirrors `EncoderConfig::bitrate_bps`. Default: 4 000 000.
    pub bitrate_bps: u32,
    /// Sender or receiver role for this config.
    pub role: TransportRole,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            udp_port: 7889,
            h264_profile: "640032".to_string(),
            bitrate_bps: 4_000_000,
            role: TransportRole::Sender,
        }
    }
}

/// Events emitted by transport adapters on the `event_tx` channel.
///
/// All variants are `Send + Sync` — the channel may be polled from any thread.
#[derive(Debug, Clone)]
pub enum TransportEvent {
    /// ICE connectivity established. Media flow expected from this moment.
    IceConnected,
    /// ICE failed or peer disconnected. The adapter does NOT auto-retry (PQ-5).
    IceFailed,
    /// A keyframe was requested by the remote peer (RTCP PLI).
    ///
    /// Observability event — the sender has already invoked
    /// `encoder.request_keyframe()` internally before emitting this event.
    KeyframeRequested,
    /// Peer drop or ICE failure with a human-readable reason string.
    ConnectionLost {
        /// Human-readable reason for the connection loss.
        reason: String,
    },
    /// Backpressure notification: one or more packets were dropped.
    PacketDropped {
        /// Cumulative dropped count at time of emission.
        count: u64,
    },
}

/// Errors produced by transport operations.
///
/// Platform errors are converted to `String` at the adapter boundary so
/// `sm-domain` never names any platform-specific type. No `#[from]` on any
/// variant — all conversions are explicit.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// The adapter is already running; `start()` called twice.
    #[error("transport already running")]
    AlreadyRunning,

    /// The adapter is not running; operation requires an active session.
    #[error("transport not running")]
    NotRunning,

    /// A configuration value was rejected (e.g., port 0, empty profile).
    #[error("invalid transport config: {0}")]
    InvalidConfig(String),

    /// An I/O error from the UDP socket or network layer.
    #[error("transport I/O error: {0}")]
    Io(String),

    /// UDP socket bind failed because the port is already in use at the OS level.
    /// Carries the port that failed to bind so callers can produce precise UX
    /// without re-parsing strings.
    ///
    /// Detected at the bind site (`Str0mVideoReceiver::start`) by matching
    /// `io::ErrorKind::AddrInUse` BEFORE the `.map_err(...)` that stringifies.
    /// Cross-platform: stdlib maps `EADDRINUSE` (Linux/macOS) and
    /// `WSAEADDRINUSE` (Windows, errno 10048) to this kind.
    #[error("UDP port {port} already in use")]
    AddrInUse { port: u16 },

    /// The signaling exchange failed (e.g., SDP parse error).
    #[error("signaling failed: {0}")]
    SignalingFailed(String),

    /// An internal transport error not covered by other variants.
    #[error("internal transport error: {0}")]
    Internal(String),
}

/// Sender-side transport port: consumes `EncodedPacket`s, emits RTP on the wire.
///
/// # Channel discipline
///
/// `start(rx, event_tx)` injects both channel ends. The sender thread owns `rx`
/// and pulls `EncodedPacket`s, then forwards each to str0m for RTP transmission.
/// `event_tx` receives `TransportEvent`s emitted asynchronously by the tick loop.
///
/// # Thread model
///
/// `new()` does NOT spawn a thread. `start()` spawns exactly one OS thread.
/// `stop()` is idempotent and joins the thread.
///
/// CALLER MUST DROP the input `Sender<EncodedPacket>` BEFORE calling `stop()`
/// so the thread's `rx.recv()` unblocks naturally.
///
/// # Encoder injection
///
/// Call `set_encoder(Arc<dyn VideoEncoder + Send + Sync>)` BEFORE `start()`.
/// The encoder is held inside the tick thread for direct PLI response
/// (call `encoder.request_keyframe()` on `Event::KeyframeRequest`).
pub trait VideoSender: Send + Sync {
    /// Construct a sender with the given configuration.
    ///
    /// Does NOT bind a socket. Does NOT spawn a thread.
    fn new(config: TransportConfig) -> Result<Self, TransportError>
    where
        Self: Sized;

    /// Inject the encoder reference for PLI feedback.
    ///
    /// MUST be called BEFORE [`start`](VideoSender::start).
    /// The encoder is held as `Arc<dyn VideoEncoder + Send + Sync>` so the
    /// tick thread can call `request_keyframe()` directly on RTCP PLI events.
    fn set_encoder(&mut self, encoder: Arc<dyn VideoEncoder + Send + Sync>);

    /// Begin sending. Spawns one OS thread that owns the str0m `Rtc` and the `UdpSocket`.
    ///
    /// Returns `Ok(())` once the thread is spawned. The tick loop runs concurrently.
    fn start(
        &mut self,
        rx: Receiver<EncodedPacket>,
        event_tx: SyncSender<TransportEvent>,
    ) -> Result<(), TransportError>;

    /// Stop the sender. Idempotent. Joins the thread.
    fn stop(&mut self) -> Result<(), TransportError>;

    /// Apply a remote SDP answer received via signaling.
    fn apply_remote_answer(&self, answer: SdpAnswer) -> Result<(), TransportError>;

    /// Add a remote ICE candidate received via signaling.
    fn add_remote_candidate(&self, cand: IceCandidate) -> Result<(), TransportError>;

    /// Produce the local SDP offer. Called once before signaling exchange.
    ///
    /// Synchronous — str0m offer creation is local computation, no I/O.
    fn create_local_offer(&self) -> Result<SdpOffer, TransportError>;

    /// Cumulative count of `EncodedPacket`s dropped due to send-side congestion.
    ///
    /// Monotonically non-decreasing. Thread-safe.
    fn dropped_frames(&self) -> u64;
}

/// Receiver-side transport port: receives RTP, emits `EncodedPacket`s.
///
/// # Channel discipline
///
/// `start(pkt_tx, event_tx)` injects both channel ends. The receiver thread
/// owns the `UdpSocket`, reassembles RTP into `EncodedPacket`s (Annex-B),
/// and forwards them via `pkt_tx.try_send`. On `Full`, increments `dropped_frames()`.
///
/// # Thread model
///
/// `new()` does NOT spawn a thread. `start()` spawns exactly one OS thread.
/// `stop()` is idempotent and joins the thread.
pub trait VideoReceiver: Send + Sync {
    /// Construct a receiver with the given configuration.
    ///
    /// Does NOT bind a socket. Does NOT spawn a thread.
    fn new(config: TransportConfig) -> Result<Self, TransportError>
    where
        Self: Sized;

    /// Begin receiving. Spawns one OS thread that owns the str0m `Rtc` and the `UdpSocket`.
    ///
    /// Packets are emitted on `pkt_tx` as `EncodedPacket` values with Annex-B data.
    fn start(
        &mut self,
        pkt_tx: SyncSender<EncodedPacket>,
        event_tx: SyncSender<TransportEvent>,
    ) -> Result<(), TransportError>;

    /// Stop the receiver. Idempotent. Joins the thread.
    fn stop(&mut self) -> Result<(), TransportError>;

    /// Apply a remote SDP offer and return the local answer.
    ///
    /// Callable before or after `start()`, but NOT after `stop()`.
    fn apply_remote_offer(&self, offer: SdpOffer) -> Result<SdpAnswer, TransportError>;

    /// Add a remote ICE candidate received via signaling.
    fn add_remote_candidate(&self, cand: IceCandidate) -> Result<(), TransportError>;

    /// Trigger a PLI to be sent to the peer at the next tick.
    fn request_keyframe(&self) -> Result<(), TransportError>;

    /// Cumulative count of `EncodedPacket`s dropped due to consumer backpressure.
    ///
    /// Monotonically non-decreasing. Thread-safe.
    fn dropped_frames(&self) -> u64;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc::{RecvTimeoutError, SyncSender, sync_channel};
    use std::time::Duration;

    // ─── Trait bound assertions: VideoSender / VideoReceiver are Send + Sync ────
    //
    // The original `transport-webrtc-str0m` change defined both traits as `Send`
    // only. Concrete adapters (`Str0mVideoSender`, `Str0mVideoReceiver`) are
    // already `Send + Sync` by composition. This compile-time assertion locks
    // the contract so callers can hold `Arc<dyn VideoSender>` / `Arc<dyn
    // VideoReceiver>` for stats polling from another thread without per-method
    // serialization. Closes verify-report SUGGESTION 3 from
    // `transport-webrtc-str0m`.
    const _: () = {
        const fn _assert_send_sync<T: Send + Sync + ?Sized>() {}
        const fn _assert_video_sender() {
            _assert_send_sync::<dyn VideoSender>();
        }
        const fn _assert_video_receiver() {
            _assert_send_sync::<dyn VideoReceiver>();
        }
    };

    // ─── FakeVideoSender ────────────────────────────────────────────────────────

    /// In-memory `VideoSender` for domain-level unit tests.
    /// No network — records calls via atomics.
    struct FakeVideoSender {
        #[allow(dead_code)]
        config: TransportConfig,
        started: Arc<AtomicBool>,
        stopped: Arc<AtomicBool>,
        dropped: Arc<AtomicU64>,
        encoder: Option<Arc<dyn VideoEncoder + Send + Sync>>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl FakeVideoSender {
        fn new_fake() -> Self {
            Self {
                config: TransportConfig::default(),
                started: Arc::new(AtomicBool::new(false)),
                stopped: Arc::new(AtomicBool::new(false)),
                dropped: Arc::new(AtomicU64::new(0)),
                encoder: None,
                handle: None,
            }
        }
    }

    impl VideoSender for FakeVideoSender {
        fn new(_config: TransportConfig) -> Result<Self, TransportError>
        where
            Self: Sized,
        {
            Ok(Self::new_fake())
        }

        fn set_encoder(&mut self, encoder: Arc<dyn VideoEncoder + Send + Sync>) {
            self.encoder = Some(encoder);
        }

        fn start(
            &mut self,
            rx: Receiver<EncodedPacket>,
            _event_tx: SyncSender<TransportEvent>,
        ) -> Result<(), TransportError> {
            if self.started.load(Ordering::Acquire) {
                return Err(TransportError::AlreadyRunning);
            }
            self.started.store(true, Ordering::Release);
            let stopped = Arc::clone(&self.stopped);
            let dropped = Arc::clone(&self.dropped);
            let handle = std::thread::spawn(move || {
                loop {
                    if stopped.load(Ordering::Acquire) {
                        break;
                    }
                    // matches FakeVideoReceiver poll cadence (5 ms) — bounds stop()
                    // regardless of whether the caller dropped the input Sender
                    match rx.recv_timeout(Duration::from_millis(5)) {
                        Ok(_pkt) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                        Err(RecvTimeoutError::Timeout) => {}
                    }
                    let _ = dropped.load(Ordering::Relaxed);
                }
            });
            self.handle = Some(handle);
            Ok(())
        }

        fn stop(&mut self) -> Result<(), TransportError> {
            self.stopped.store(true, Ordering::Release);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
            Ok(())
        }

        fn apply_remote_answer(&self, _answer: SdpAnswer) -> Result<(), TransportError> {
            Ok(())
        }

        fn add_remote_candidate(&self, _cand: IceCandidate) -> Result<(), TransportError> {
            Ok(())
        }

        fn create_local_offer(&self) -> Result<SdpOffer, TransportError> {
            Ok(SdpOffer("v=0\r\n".to_string()))
        }

        fn dropped_frames(&self) -> u64 {
            self.dropped.load(Ordering::Relaxed)
        }
    }

    // ─── FakeVideoReceiver ──────────────────────────────────────────────────────

    /// In-memory `VideoReceiver` for domain-level unit tests.
    struct FakeVideoReceiver {
        stopped: Arc<AtomicBool>,
        dropped: Arc<AtomicU64>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl FakeVideoReceiver {
        fn new_fake() -> Self {
            Self {
                stopped: Arc::new(AtomicBool::new(false)),
                dropped: Arc::new(AtomicU64::new(0)),
                handle: None,
            }
        }
    }

    impl VideoReceiver for FakeVideoReceiver {
        fn new(_config: TransportConfig) -> Result<Self, TransportError>
        where
            Self: Sized,
        {
            Ok(Self::new_fake())
        }

        fn start(
            &mut self,
            _pkt_tx: SyncSender<EncodedPacket>,
            _event_tx: SyncSender<TransportEvent>,
        ) -> Result<(), TransportError> {
            let stopped = Arc::clone(&self.stopped);
            let handle = std::thread::spawn(move || {
                while !stopped.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(5));
                }
            });
            self.handle = Some(handle);
            Ok(())
        }

        fn stop(&mut self) -> Result<(), TransportError> {
            self.stopped.store(true, Ordering::Release);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
            Ok(())
        }

        fn apply_remote_offer(&self, _offer: SdpOffer) -> Result<SdpAnswer, TransportError> {
            Ok(SdpAnswer("v=0\r\n".to_string()))
        }

        fn add_remote_candidate(&self, _cand: IceCandidate) -> Result<(), TransportError> {
            Ok(())
        }

        fn request_keyframe(&self) -> Result<(), TransportError> {
            Ok(())
        }

        fn dropped_frames(&self) -> u64 {
            self.dropped.load(Ordering::Relaxed)
        }
    }

    // ─── S1.2: FakeVideoSender start/stop lifecycle ──────────────────────────────

    #[test]
    fn fake_video_sender_lifecycle_s1_2() {
        let mut sender = FakeVideoSender::new_fake();
        let (pkt_tx, pkt_rx) = sync_channel::<EncodedPacket>(TRANSPORT_CHANNEL_CAPACITY);
        let (event_tx, _event_rx) = sync_channel::<TransportEvent>(TRANSPORT_CHANNEL_CAPACITY);
        sender.start(pkt_rx, event_tx).unwrap();
        assert_eq!(sender.dropped_frames(), 0);
        // drop sender side before stop so thread unblocks
        drop(pkt_tx);
        sender.stop().unwrap();
    }

    // ─── S1.3: FakeVideoSender idempotent stop ────────────────────────────────

    #[test]
    fn fake_video_sender_stop_idempotent_s1_3() {
        let mut sender = FakeVideoSender::new_fake();
        // stop on never-started sender
        sender.stop().unwrap();
        // start + stop + stop
        let (_pkt_tx, pkt_rx) = sync_channel::<EncodedPacket>(TRANSPORT_CHANNEL_CAPACITY);
        let (event_tx, _event_rx) = sync_channel::<TransportEvent>(TRANSPORT_CHANNEL_CAPACITY);
        sender.started.store(false, Ordering::Release);
        sender.stopped.store(false, Ordering::Release);
        sender.start(pkt_rx, event_tx).unwrap();
        sender.stop().unwrap();
        sender.stop().unwrap(); // second stop must not panic
    }

    // ─── S2.1: FakeVideoReceiver apply_remote_offer ──────────────────────────

    #[test]
    fn fake_video_receiver_apply_remote_offer_s2_1() {
        let receiver = FakeVideoReceiver::new_fake();
        let result = receiver.apply_remote_offer(SdpOffer("v=0\r\n".to_string()));
        assert!(result.is_ok(), "apply_remote_offer must return Ok");
        let answer = result.unwrap();
        // SdpAnswer must be a newtype wrapping a String (from signaling module)
        let _ = answer; // just ensure it exists and compiles
    }

    // ─── S2.2: FakeVideoReceiver idempotent stop ──────────────────────────────

    #[test]
    fn fake_video_receiver_stop_idempotent_s2_2() {
        let mut receiver = FakeVideoReceiver::new_fake();
        receiver.stop().unwrap();
        receiver.stop().unwrap(); // must not panic
    }

    // ─── S4.1: TransportConfig::default() values ──────────────────────────────

    #[test]
    fn transport_config_default_values_s4_1() {
        let cfg = TransportConfig::default();
        assert_eq!(cfg.udp_port, 7889, "default udp_port must be 7889 (PQ-1)");
        assert_eq!(
            cfg.h264_profile, "640032",
            "default h264_profile must be '640032' (PQ-3)"
        );
    }

    // ─── S4.2: TransportEvent is Send + Sync ──────────────────────────────────

    #[test]
    fn transport_event_is_send_sync_s4_2() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<TransportEvent>();
    }

    // ─── S4.3: TransportError Display contains keyword ────────────────────────

    #[test]
    fn transport_error_display_contains_keyword_s4_3() {
        let err = TransportError::InvalidConfig("port 0 is not valid".to_string());
        let msg = format!("{err}");
        assert!(
            msg.contains("port"),
            "TransportError::InvalidConfig display must contain 'port', got: {msg}"
        );
    }

    // ─── S14.1: TRANSPORT_CHANNEL_CAPACITY == 4 ──────────────────────────────

    #[test]
    fn transport_channel_capacity_value_s14_1() {
        assert_eq!(
            TRANSPORT_CHANNEL_CAPACITY, 4,
            "TRANSPORT_CHANNEL_CAPACITY must be 4 (range [4,8])"
        );
    }

    // ─── B1 RED: TransportError::AddrInUse (R1.1, R1.2) ─────────────────────────

    #[test]
    fn transport_error_addr_in_use_display_carries_port() {
        let err = TransportError::AddrInUse { port: 7889 };
        let msg = format!("{err}");
        assert_eq!(msg, "UDP port 7889 already in use");
    }

    #[test]
    fn transport_error_addr_in_use_debug_contains_variant_name() {
        let err = TransportError::AddrInUse { port: 7889 };
        let dbg = format!("{err:?}");
        assert!(
            dbg.contains("AddrInUse"),
            "Debug must contain 'AddrInUse', got: {dbg}"
        );
        assert!(
            dbg.contains("7889"),
            "Debug must contain '7889', got: {dbg}"
        );
    }
}
