//! str0m-backed video receiver adapter.
//!
//! [`Str0mVideoReceiver`] implements [`VideoReceiver`] using the str0m SansIO WebRTC stack.
//! It is cross-platform (no `cfg` gate) per PQ-9.
//!
//! # Thread model
//!
//! - `new()`: creates `Rtc`; no socket, no thread.
//! - `apply_remote_offer(&self, offer)`: accepts a remote SDP offer synchronously using
//!   the pre-constructed `Rtc` and returns the local `SdpAnswer`. May be called before or
//!   after `start()`.
//! - `start(pkt_tx, event_tx)`: binds the `UdpSocket`, moves `Rtc` into one OS thread
//!   that runs the SansIO tick loop with media demux + PLI emission.
//! - `stop()`: sets the `AtomicBool` stop flag and joins the thread. Idempotent.
//! - `Drop`: calls `stop()` to prevent leaked threads on panic or forgotten stop.
//!
//! # Batch 4 additions vs Batch 3
//!
//! - `Rtc` is created in `new()` (not `start()`) so that `apply_remote_offer()` works
//!   as a synchronous pre-thread call, mirroring how `Str0mVideoSender::create_local_offer`
//!   is synchronous.
//! - `apply_remote_offer` calls `rtc.sdp_api().accept_offer(offer)` → returns `SdpAnswer`.
//! - Tick loop handles `ReceiverControl::AddCandidate` → `rtc.add_remote_candidate(c)`.
//! - Tick loop handles `ReceiverControl::RequestKeyframe` → `rtc.writer(mid).request_keyframe(...)`.
//! - Tick loop handles `Event::MediaData` → reconstruct Annex-B → emit `EncodedPacket`.
//! - `Event::MediaAdded` captures the media `Mid` for subsequent PLI calls.
//! - `recv_from` timeout capped at 200 ms so `stop()` unblocks quickly (S14.4).

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use str0m::media::{KeyframeRequestKind, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};

use sm_domain::encode::EncodedPacket;
use sm_domain::signaling::{IceCandidate, SdpAnswer, SdpOffer};
use sm_domain::transport::{
    TRANSPORT_CHANNEL_CAPACITY, TransportConfig, TransportError, TransportEvent, VideoReceiver,
};

use crate::diagnostics::qsv_ledger::{LedgerMarker, MarkerError, TransportLedgerProbe};
use crate::transport::annex_b::{contains_idr_nal, reconstruct_annex_b};

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
    /// Process a remote SDP offer on the tick thread and reply with the answer.
    ///
    /// Used by the post-start path of `apply_remote_offer` (design §3.2).
    /// The `reply` sender is a `SyncSender` with capacity 1 so that `try_send`
    /// is non-blocking on the tick thread even if the caller already timed out.
    ApplyOffer {
        offer: SdpOffer,
        reply: std::sync::mpsc::SyncSender<Result<SdpAnswer, TransportError>>,
    },
}

// ─── Pre-negotiation state ───────────────────────────────────────────────────

/// Holds the `Rtc` instance before `start()` is called (and during the tick thread).
///
/// Protected by a `Mutex` so that `apply_remote_offer(&self)` can call `&mut Rtc`
/// without requiring `&mut self` on the public API method.
struct ReceiverPreNeg {
    rtc: Rtc,
    /// Media identifier captured from `Event::MediaAdded`; needed for PLI.
    mid: Option<Mid>,
}

// ─── Ledger epoch state ───────────────────────────────────────────────────────
#[derive(Default)]
struct ReceiverLedgerState {
    active_marker: Option<LedgerMarker>,
}

// ─── Shared state ────────────────────────────────────────────────────────────

/// Cross-thread state shared between the caller and the SansIO tick thread.
struct ReceiverShared {
    /// Raised by `stop()` / `Drop`; checked at the top of each tick iteration.
    stop: AtomicBool,
    /// Cumulative count of `EncodedPacket`s dropped due to consumer backpressure.
    dropped: AtomicU64,
    /// Monotonically increasing sequence counter reset to 0 on `start()`.
    seq: AtomicU64,
    /// One-shot guard for the first-media `TransportEvent::MediaData` emit
    /// (media-arrival watchdog, design #971 §D4/O4).
    ///
    /// `false` on construction → set `true` by `emit_first_media_once` on the first
    /// `str0m::Event::MediaData`. Because this lives on `ReceiverShared` (a fresh
    /// instance per `new()` / per generation), each rebuilt receiver re-arms it
    /// naturally — it is NOT a process-wide static, so a new generation emits a
    /// fresh `MediaData` and the watchdog disarms per generation.
    media_emitted: AtomicBool,
}

/// Receiver-owned values moved into the dedicated SansIO tick loop.
struct ReceiverLoopContext {
    state: Arc<ReceiverShared>,
    probe: Option<Arc<TransportLedgerProbe>>,
}

impl ReceiverShared {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            stop: AtomicBool::new(false),
            dropped: AtomicU64::new(0),
            seq: AtomicU64::new(0),
            media_emitted: AtomicBool::new(false),
        })
    }
}

/// Emit `TransportEvent::MediaData` exactly ONCE per generation, on the first media.
///
/// Media-arrival watchdog signal source (design #971 §D4/O4): the drain arms a
/// deadline after a rebuild reports success; the FIRST media on the new transport
/// generation disarms it via this event. Subsequent media on the same generation
/// is silent (one-shot) so the channel is not flooded — the actual `EncodedPacket`
/// still flows on the packet channel, unchanged.
///
/// The guard uses `compare_exchange` (Acquire/Relaxed) so concurrent first-media
/// observations still emit exactly once. The `try_send` is best-effort: a full or
/// closed channel is ignored (the watchdog tolerates a missed disarm by re-arming
/// — never blocks the tick loop).
fn emit_first_media_once(media_emitted: &AtomicBool, event_tx: &SyncSender<TransportEvent>) {
    if media_emitted
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_ok()
    {
        let _ = event_tx.try_send(TransportEvent::MediaData);
    }
}

// ─── Str0mVideoReceiver ──────────────────────────────────────────────────────

/// str0m-backed video receiver. Implements [`VideoReceiver`].
///
/// Cross-platform — no `#[cfg(target_os = "windows")]` gate (PQ-9).
///
/// # Lifecycle
///
/// 1. `new(config)` — creates `Rtc`; no socket, no thread.
/// 2. `apply_remote_offer(offer)` — synchronous; can be called before or after `start()`.
/// 3. `start(pkt_tx, event_tx)` — binds UDP socket, spawns one OS thread.
/// 4. `stop()` / `Drop` — sets stop flag, joins thread. Idempotent.
pub struct Str0mVideoReceiver {
    /// Original transport configuration.
    config: TransportConfig,
    /// Pre-negotiation state: Rtc + Mid. Guarded by Mutex for `&self` access on
    /// `apply_remote_offer`. Taken out (→ None) when `start()` moves Rtc to tick thread.
    pre_neg: Mutex<Option<ReceiverPreNeg>>,
    /// Shared atomic state between caller and tick thread.
    state: Arc<ReceiverShared>,
    ledger: Mutex<ReceiverLedgerState>,
    transport_ledger_probe: Mutex<Option<Arc<TransportLedgerProbe>>>,
    /// Control inbox: caller → tick thread. Created in `start()`.
    control_tx: Option<SyncSender<ReceiverControl>>,
    /// Join handle for the tick thread. `Some` while running.
    handle: Option<JoinHandle<()>>,
    /// Effective local socket address after `start()`. `None` before start.
    local_addr: Option<std::net::SocketAddr>,
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
// `Mutex<Option<ReceiverPreNeg>>` is Send when `ReceiverPreNeg: Send`
// (Rtc is Send per str0m guarantee).
unsafe impl Send for Str0mVideoReceiver {}
// SAFETY: Methods that take `&self` either read atomics (Sync), clone SyncSender (Sync),
// or acquire the Mutex<Option<ReceiverPreNeg>> (Mutex is Sync). No `&self` path reaches
// a `!Sync` field.
unsafe impl Sync for Str0mVideoReceiver {}

impl VideoReceiver for Str0mVideoReceiver {
    /// Construct a receiver with the given configuration.
    ///
    /// Creates an `Rtc` instance synchronously so that `apply_remote_offer()` works
    /// without a thread.
    fn new(config: TransportConfig) -> Result<Self, TransportError>
    where
        Self: Sized,
    {
        let crypto = str0m::crypto::from_feature_flags();
        let rtc = Rtc::builder()
            .set_crypto_provider(Arc::new(crypto))
            .build(Instant::now());

        let pre_neg = ReceiverPreNeg { rtc, mid: None };

        Ok(Self {
            config,
            pre_neg: Mutex::new(Some(pre_neg)),
            state: ReceiverShared::new(),
            ledger: Mutex::new(ReceiverLedgerState::default()),
            transport_ledger_probe: Mutex::new(None),
            control_tx: None,
            handle: None,
            local_addr: None,
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

        // Reset stop flag and sequence counter in case this receiver is restarted.
        self.state.stop.store(false, Ordering::Release);
        self.state.seq.store(0, Ordering::Release);

        let bind_addr = format!("0.0.0.0:{}", self.config.udp_port);
        let udp = UdpSocket::bind(&bind_addr)
            .map_err(|e| classify_bind_error(e, self.config.udp_port))?;

        self.start_from_socket(udp, pkt_tx, event_tx)
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

    /// Accept a remote SDP offer and return the local SDP answer.
    ///
    /// Implements a dual-path strategy so the method is callable both before and
    /// after `start()` (spec R6.3, design §3.2):
    ///
    /// - **Path A (pre-start)**: `Rtc` is still in `pre_neg`. Process the offer
    ///   synchronously on the caller's thread. The `pre_neg` lock is released
    ///   before any blocking call to prevent potential future deadlocks.
    /// - **Path B (post-start)**: `Rtc` has been moved to the tick thread. Send
    ///   an `ApplyOffer` control message with a reply channel, then block with a
    ///   2-second timeout. This is generous for an in-process operation but
    ///   bounded so a dead/wedged tick thread cannot hang the caller forever.
    fn apply_remote_offer(&self, offer: SdpOffer) -> Result<SdpAnswer, TransportError> {
        self.apply_remote_offer_for_epoch(offer, 1)
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

impl Str0mVideoReceiver {
    /// Attaches an inert probe for external test-support contract compilation.
    #[cfg(feature = "test-support")]
    pub fn install_transport_ledger_probe_for_test(&self, probe: Arc<TransportLedgerProbe>) {
        if let Ok(mut attached) = self.transport_ledger_probe.lock() {
            *attached = Some(probe);
        }
    }

    fn receiver_loop_context(&self) -> ReceiverLoopContext {
        let probe = self
            .transport_ledger_probe
            .lock()
            .ok()
            .and_then(|attached| attached.clone());

        ReceiverLoopContext {
            state: Arc::clone(&self.state),
            probe,
        }
    }

    pub fn apply_remote_offer_for_epoch(
        &self,
        offer: SdpOffer,
        expected_epoch: u8,
    ) -> Result<SdpAnswer, TransportError> {
        let validated = validate_and_strip_qsv_ledger_offer(&offer.0, u64::from(expected_epoch));
        let (offer, marker) = match validated {
            Ok(accepted) => (SdpOffer(accepted.offer), accepted.marker),
            Err(_) => (SdpOffer(strip_qsv_ledger_marker_attributes(&offer.0)), None),
        };
        // Epoch rotation clears diagnostic state before a new offer is accepted.
        if let Ok(mut ledger) = self.ledger.lock() {
            ledger.active_marker = None;
        }
        let pre_start = {
            let mut guard = self
                .pre_neg
                .lock()
                .map_err(|e| TransportError::Internal(format!("mutex poisoned: {e}")))?;
            guard
                .as_mut()
                .map(|pn| apply_offer_to_rtc(&mut pn.rtc, offer.clone()))
        };
        let result = match pre_start {
            Some(result) => result,
            None => {
                let tx = self.control_tx.as_ref().ok_or(TransportError::NotRunning)?;
                let (reply, rx) = std::sync::mpsc::sync_channel(1);
                tx.try_send(ReceiverControl::ApplyOffer { offer, reply })
                    .map_err(|_| {
                        TransportError::Internal("control inbox full or disconnected".into())
                    })?;
                rx.recv_timeout(Duration::from_secs(2)).map_err(|e| {
                    TransportError::Internal(format!("apply_remote_offer reply timeout: {e}"))
                })?
            }
        };
        if result.is_ok()
            && let Some(attribute) = marker
            && let Ok(marker) = LedgerMarker::parse(&attribute)
            && let Ok(mut ledger) = self.ledger.lock()
        {
            ledger.active_marker = Some(marker);
        }
        result
    }

    /// Return the effective local socket address after `start()`.
    ///
    /// Returns `None` before `start()` is called. Used by integration tests and
    /// signaling adapters to discover the bound ephemeral port for ICE candidate exchange.
    pub fn local_addr(&self) -> Option<std::net::SocketAddr> {
        self.local_addr
    }

    /// Return the ICE host candidate address for this receiver.
    ///
    /// Returns `Some(addr)` after a successful `start()` or `start_with_socket()`, where
    /// `addr.port()` matches the bound UDP socket port and `addr.ip()` is a non-loopback
    /// IPv4 address.
    ///
    /// When `effective_local_addr` is a loopback address (`127.0.0.1`) — which happens
    /// when the socket was bound to `0.0.0.0` — this method substitutes the first
    /// non-loopback IPv4 NIC address from `enumerate_local_ipv4()`, preserving the port.
    ///
    /// Returns `None` in these cases:
    /// - `start()` / `start_with_socket()` has not been called yet.
    /// - No non-loopback IPv4 NIC is available.
    ///
    /// MUST NOT mutate `self.local_addr` — that field is used as `Input::Receive {
    /// destination }` inside str0m's tick loop and must remain the bind address.
    pub fn candidate_addr(&self) -> Option<std::net::SocketAddr> {
        let local = self.local_addr?; // None before start()
        if !local.ip().is_loopback() {
            return Some(local); // already a routable address — use it directly
        }
        // local.ip() is loopback: substitute the first non-loopback IPv4 NIC,
        // preserving the bound port so STUN reaches the correct UDP socket.
        let nic = crate::transport::enumerate_local_ipv4()
            .into_iter()
            .next()?;
        Some(std::net::SocketAddr::new(
            std::net::IpAddr::V4(nic),
            local.port(),
        ))
    }

    /// Begin receiving with an externally-bound `UdpSocket`. Mirrors `start()` but
    /// skips the internal `UdpSocket::bind`.
    ///
    /// Used by the Tauri shell's TOCTOU-hardened path (`build_production_bundle`)
    /// where the socket is acquired up-front by `bind_probe` and threaded here
    /// through `BindCtx`. The trait `start()` remains for callers that bind
    /// ephemeral ports (port 0 in tests/examples).
    ///
    /// Returns `Err(AlreadyRunning)` if called while a previous run is active.
    pub fn start_with_socket(
        &mut self,
        udp: UdpSocket,
        pkt_tx: SyncSender<EncodedPacket>,
        event_tx: SyncSender<TransportEvent>,
    ) -> Result<(), TransportError> {
        if self.handle.is_some() {
            return Err(TransportError::AlreadyRunning);
        }

        // Reset stop flag and sequence counter — matches start() reset path.
        self.state.stop.store(false, Ordering::Release);
        self.state.seq.store(0, Ordering::Release);

        self.start_from_socket(udp, pkt_tx, event_tx)
    }

    /// Post-bind setup: extract the `Rtc`, compute the effective local address,
    /// add the local ICE candidate, spawn the tick thread, and store the handle.
    ///
    /// Called by both `start()` (after its own `UdpSocket::bind`) and
    /// `start_with_socket()` (with a prebound socket from `bind_probe`).
    /// This is the SINGLE source of truth for post-bind initialization.
    fn start_from_socket(
        &mut self,
        udp: UdpSocket,
        pkt_tx: SyncSender<EncodedPacket>,
        event_tx: SyncSender<TransportEvent>,
    ) -> Result<(), TransportError> {
        let local_addr = udp
            .local_addr()
            .map_err(|e| TransportError::Io(e.to_string()))?;

        // Take the Rtc out of pre_neg and move it into the thread.
        let pre_neg = {
            let mut guard = self.pre_neg.lock().unwrap();
            guard
                .take()
                .ok_or_else(|| TransportError::Internal("Rtc already moved to thread".into()))?
        };

        // Compute the effective local address for ICE candidate registration and
        // for the `Input::Receive { destination }` field in the tick loop.
        // When bound to the wildcard address (0.0.0.0), fall back to 127.0.0.1
        // with the actual bound port.
        let effective_local_addr: std::net::SocketAddr = if local_addr.ip().is_unspecified() {
            format!("127.0.0.1:{}", local_addr.port()).parse().unwrap()
        } else {
            local_addr
        };

        let mut rtc_tmp = pre_neg.rtc;
        if let Ok(cand) = Candidate::host(effective_local_addr, "udp") {
            rtc_tmp.add_local_candidate(cand);
        }

        let pre_neg_with_candidate = ReceiverPreNeg {
            rtc: rtc_tmp,
            mid: pre_neg.mid,
        };

        let (ctrl_tx, ctrl_rx) =
            std::sync::mpsc::sync_channel::<ReceiverControl>(TRANSPORT_CHANNEL_CAPACITY);
        self.control_tx = Some(ctrl_tx);

        let context = self.receiver_loop_context();

        let handle = std::thread::Builder::new()
            .name("sm-transport-receiver".into())
            .spawn(move || {
                run_receiver_loop(
                    pre_neg_with_candidate,
                    udp,
                    effective_local_addr,
                    pkt_tx,
                    event_tx,
                    ctrl_rx,
                    context,
                );
            })
            .map_err(|e| TransportError::Internal(format!("thread spawn failed: {e}")))?;

        self.handle = Some(handle);
        // Store the effective local address so callers can retrieve it for candidate exchange.
        self.local_addr = Some(effective_local_addr);
        Ok(())
    }
}

// ─── SDP helper ─────────────────────────────────────────────────────────────

/// Parse a domain [`SdpOffer`], feed it to an [`Rtc`], and return the domain
/// [`SdpAnswer`].
///
/// Shared by both paths of [`Str0mVideoReceiver::apply_remote_offer`]:
/// - **Path A** (pre-start): called directly on the caller's thread.
/// - **Path B** (post-start): called on the tick thread after the offer arrives
///   via the `ReceiverControl::ApplyOffer` message.
struct ValidatedLedgerOffer {
    offer: String,
    marker: Option<String>,
}

fn normalized_sdp_origin_identity(sdp: &str) -> Option<String> {
    sdp.lines().find_map(|line| {
        let mut fields = line.strip_prefix("o=")?.split_whitespace();
        let _username = fields.next()?;
        Some(format!("{} {}", fields.next()?, fields.next()?))
    })
}

fn strip_qsv_ledger_marker_attributes(sdp: &str) -> String {
    sdp.split_inclusive('\n')
        .filter(|line| {
            !line
                .trim_end_matches(['\r', '\n'])
                .starts_with("a=x-sm-qsv-ledger:")
        })
        .collect()
}
fn validate_and_strip_qsv_ledger_offer(
    sdp: &str,
    expected_epoch: u64,
) -> Result<ValidatedLedgerOffer, MarkerError> {
    let markers: Vec<&str> = sdp
        .lines()
        .filter(|line| line.starts_with("a=x-sm-qsv-ledger:"))
        .collect();
    if markers.is_empty() {
        return Ok(ValidatedLedgerOffer {
            offer: sdp.to_owned(),
            marker: None,
        });
    }
    if markers.len() != 1 {
        return Err(MarkerError::Malformed);
    }
    let attribute = markers[0];
    let marker = LedgerMarker::parse(attribute)?;
    let session_id = normalized_sdp_origin_identity(sdp).ok_or(MarkerError::Malformed)?;
    marker.validate_for_session(&session_id)?;
    if marker != LedgerMarker::new(&session_id, expected_epoch) {
        return Err(MarkerError::Malformed);
    }
    Ok(ValidatedLedgerOffer {
        offer: strip_qsv_ledger_marker_attributes(sdp),
        marker: Some(attribute.to_owned()),
    })
}

fn apply_offer_to_rtc(rtc: &mut Rtc, offer: SdpOffer) -> Result<SdpAnswer, TransportError> {
    let str0m_offer = str0m::change::SdpOffer::from_sdp_string(&offer.0)
        .map_err(|e| TransportError::Internal(format!("SDP offer parse failed: {e}")))?;
    let str0m_answer = rtc
        .sdp_api()
        .accept_offer(str0m_offer)
        .map_err(|e| TransportError::Internal(format!("accept_offer failed: {e}")))?;
    Ok(SdpAnswer(str0m_answer.to_string()))
}

// ─── Tick loop ───────────────────────────────────────────────────────────────

// ─── GapStats — RECV-OBS-1 pure seam (Slice 2) ───────────────────────────────
//
// Accumulates inter-arrival gap data for a single per-second measurement window.
// Lives on the tick thread (single-threaded); no allocation, no locks.
// Mirrors ConvertStats (encode/windows_mft.rs) in structure and scope.
//
// NOTE: The per-second window boundary is checked event-driven (at each MediaData),
// matching the capture_fps pattern in capture/windows.rs. If media stalls, no
// emission fires that second — that is intentional, not a bug.

#[derive(Default)]
struct GapStats {
    count: u32,
    total_us: u64,
    max_us: u64,
}

/// Window state for RECV-OBS-1. Bundled so `handle_receiver_event` stays within
/// clippy's 7-argument limit.
struct RecvGapState {
    stats: GapStats,
    window_start: Instant,
    last_arrival: Option<Instant>,
}

impl RecvGapState {
    fn new() -> Self {
        Self {
            stats: GapStats::default(),
            window_start: Instant::now(),
            last_arrival: None,
        }
    }
}

impl GapStats {
    /// Record one inter-arrival gap.
    fn record(&mut self, gap: Duration) {
        let us = gap.as_micros() as u64;
        self.count += 1;
        self.total_us += us;
        if us > self.max_us {
            self.max_us = us;
        }
    }

    /// Frames per second over the given elapsed window. Returns 0.0 when no frames recorded.
    fn receive_fps(&self, window: Duration) -> f64 {
        if self.count == 0 {
            return 0.0;
        }
        self.count as f64 / window.as_secs_f64()
    }

    /// Mean inter-arrival gap in milliseconds. Returns 0.0 when no frames recorded.
    fn mean_gap_ms(&self) -> f64 {
        if self.count == 0 {
            return 0.0;
        }
        self.total_us as f64 / self.count as f64 / 1000.0
    }

    /// Maximum inter-arrival gap in milliseconds.
    fn max_gap_ms(&self) -> f64 {
        self.max_us as f64 / 1000.0
    }

    /// Reset all accumulators (call after per-second emission).
    fn reset(&mut self) {
        self.count = 0;
        self.total_us = 0;
        self.max_us = 0;
    }
}

/// SansIO tick loop for `Str0mVideoReceiver`.
///
/// Runs on the dedicated OS thread spawned by `start()`.
fn run_receiver_loop(
    mut pre_neg: ReceiverPreNeg,
    udp: UdpSocket,
    local_addr: SocketAddr,
    pkt_tx: SyncSender<EncodedPacket>,
    event_tx: SyncSender<TransportEvent>,
    ctrl_rx: Receiver<ReceiverControl>,
    context: ReceiverLoopContext,
) {
    let state = context.state;
    let _probe = context.probe;
    let mut buf = vec![0u8; 2048];
    let rtc = &mut pre_neg.rtc;
    // Instrumentation (HW gate): once-per-generation flag + start instant so the
    // FIRST inbound datagram is logged exactly once with elapsed time. This
    // distinguishes a dead socket (no datagram ever) from an ICE-level failure
    // (datagrams arrive but no working pair).
    let mut first_datagram_logged = false;
    let loop_start = Instant::now();
    // RECV-OBS-1: arrival-gap accumulator (Slice 2).
    let mut gap_state = RecvGapState::new();

    loop {
        // ── 1. Stop flag ──────────────────────────────────────────────────
        if state.stop.load(Ordering::Acquire) {
            break;
        }

        // ── 2. Drain control inbox ────────────────────────────────────────
        while let Ok(msg) = ctrl_rx.try_recv() {
            match msg {
                ReceiverControl::AddCandidate(cand) => {
                    // Candidates are JSON-serialised str0m::Candidate values.
                    if let Ok(c) = serde_json::from_str::<Candidate>(&cand.0) {
                        rtc.add_remote_candidate(c);
                    }
                    // Silently ignore un-parseable candidates.
                }
                ReceiverControl::RequestKeyframe => {
                    // Send a PLI to the remote sender.
                    if let Some(mid) = pre_neg.mid
                        && let Some(mut writer) = rtc.writer(mid)
                    {
                        let _ = writer.request_keyframe(None, KeyframeRequestKind::Pli);
                    }
                }
                ReceiverControl::ApplyOffer { offer, reply } => {
                    // Process the offer on this thread (which owns `rtc`) and
                    // send the result back to the caller.
                    // If the caller already timed out and dropped `reply`, the
                    // send silently fails — that is acceptable.
                    let result = apply_offer_to_rtc(rtc, offer);
                    let _ = reply.try_send(result);
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
                    handle_receiver_event(
                        ev,
                        &mut pre_neg.mid,
                        &state,
                        &pkt_tx,
                        &event_tx,
                        &mut gap_state,
                    );
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
        // Cap at 200 ms so that stop() unblocks quickly (S14.4).
        let now = Instant::now();
        let remaining = deadline
            .checked_duration_since(now)
            .unwrap_or(Duration::from_millis(1));
        let timeout = remaining
            .min(Duration::from_millis(200))
            .max(Duration::from_millis(1));

        if let Err(e) = udp.set_read_timeout(Some(timeout)) {
            let _ = event_tx.try_send(TransportEvent::ConnectionLost {
                reason: format!("set_read_timeout failed: {e}"),
            });
            break;
        }

        match udp.recv_from(&mut buf) {
            Ok((n, source)) => {
                // Instrumentation (HW gate, log #5): log the FIRST inbound
                // datagram (typically a STUN binding request) with elapsed time.
                // Proves whether ANY packet reaches the rebuilt socket → dead
                // socket vs ICE-level failure.
                if !first_datagram_logged {
                    first_datagram_logged = true;
                    eprintln!(
                        "[sm-receiver-tick] first datagram in from {source} at +{}ms (n={n})",
                        loop_start.elapsed().as_millis()
                    );
                }
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
            Err(ref e) if e.kind() == std::io::ErrorKind::ConnectionReset => {
                // Windows-specific: WSAECONNRESET on a UDP socket surfaces a
                // queued ICMP "destination unreachable" from a previous
                // send_to. UDP is connectionless — this is advisory and the
                // socket remains usable. Linux silently drops these.
                eprintln!(
                    "[sm-receiver-tick] ignoring transient ICMP-related recv_from error (WSAECONNRESET): {e}"
                );
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
    // Thread exits cleanly — socket dropped here.
}

/// Dispatch str0m events for the receiver.
///
/// `gap_state` carries the RECV-OBS-1 accumulator (Slice 2); bundled into one
/// `&mut RecvGapState` to stay within clippy's 7-argument limit.
fn handle_receiver_event(
    ev: Event,
    mid_slot: &mut Option<Mid>,
    state: &ReceiverShared,
    pkt_tx: &SyncSender<EncodedPacket>,
    event_tx: &SyncSender<TransportEvent>,
    gap_state: &mut RecvGapState,
) {
    match ev {
        // `Connected` fires when at least one candidate pair is working but gathering
        // may still be in progress. `Completed` fires when the best pair is selected
        // and gathering is done. With a single candidate pair (loopback tests, most
        // prod scenarios), the state jumps directly to `Completed`, skipping `Connected`.
        // We map both to `TransportEvent::IceConnected`.
        Event::IceConnectionStateChange(state) => {
            // Instrumentation (HW gate, log #6): log EVERY ICE state transition
            // (incl. Checking/Disconnected/New), not just Connected — shows if
            // ICE stalls in `Checking` on the rebuilt Rtc.
            eprintln!("[sm-receiver] ICE state -> {state:?}");
            match state {
                IceConnectionState::Connected | IceConnectionState::Completed => {
                    let _ = event_tx.try_send(TransportEvent::IceConnected);
                }
                IceConnectionState::Disconnected => {
                    let _ = event_tx.try_send(TransportEvent::IceFailed);
                }
                _ => {}
            }
        }
        Event::MediaAdded(added) => {
            // Instrumentation (HW gate, log #4 — HIGHEST VALUE): proves whether
            // MediaAdded ever fires on the rebuilt Rtc post-reconnect.
            eprintln!(
                "[sm-receiver] MediaAdded mid={:?} on rebuilt Rtc",
                added.mid
            );
            // Capture the mid so we can send PLI later.
            *mid_slot = Some(added.mid);
        }
        Event::MediaData(media) => {
            // RECV-OBS-1 (Slice 2): record inter-arrival gap and emit per-second
            // diagnostic line. Event-driven cadence: fires only when MediaData arrives;
            // a stalled second produces no line (intentional, not a bug — mirrors
            // capture_fps in capture/windows.rs). No allocation, no lock.
            let now = Instant::now();
            if let Some(prev) = gap_state.last_arrival {
                gap_state.stats.record(now.duration_since(prev));
            }
            gap_state.last_arrival = Some(now);
            if gap_state.window_start.elapsed() >= Duration::from_secs(1) {
                let window = gap_state.window_start.elapsed();
                // RECV-OBS-1 scenario 3: skip emission when no frames arrived in the
                // window (count == 0). The window still resets so the cadence stays
                // event-driven; last_arrival persists for gap measurement continuity.
                if gap_state.stats.count > 0 {
                    eprintln!(
                        "[sm-receiver-gap] receive_fps={:.1} max_gap_ms={:.1} mean_gap_ms={:.1} frames={}",
                        gap_state.stats.receive_fps(window),
                        gap_state.stats.max_gap_ms(),
                        gap_state.stats.mean_gap_ms(),
                        gap_state.stats.count,
                    );
                }
                gap_state.stats.reset();
                gap_state.window_start = now;
                // last_arrival persists across windows so the gap straddling a boundary
                // is not lost (next gap is measured from this arrival).
            }

            // Media-arrival watchdog (design #971 §D4/O4): signal the FIRST media of
            // this generation so the post-rebuild watchdog in the drain can disarm.
            // One-shot per generation (guarded by `state.media_emitted`); subsequent
            // media is silent. Does NOT affect packet delivery below.
            emit_first_media_once(&state.media_emitted, event_tx);

            // Reconstruct Annex-B from whatever framing str0m used.
            // str0m's H264Depacketizer with is_avc=false outputs Annex-B directly;
            // reconstruct_annex_b detects this and passes through without double-prefix.
            let annex_b = reconstruct_annex_b(&media.data);
            let is_keyframe = media.is_keyframe() || contains_idr_nal(&annex_b);

            let pkt = EncodedPacket {
                data: Arc::from(annex_b.as_slice()),
                is_keyframe,
                timestamp: Duration::from_micros(media.time.as_micros()),
                sequence: state.seq.fetch_add(1, Ordering::Relaxed),
            };

            if pkt_tx.try_send(pkt).is_err() {
                state.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        _ => {}
    }
}

// ─── B2 — classify_bind_error: AddrInUse detection helper (R1.4) ─────────────

/// Translate a `UdpSocket::bind` `io::Error` into the appropriate `TransportError`.
///
/// Checks `e.kind() == ErrorKind::AddrInUse` BEFORE stringifying. On `AddrInUse`,
/// returns `TransportError::AddrInUse { port }`. On any other kind, falls through to
/// the legacy `TransportError::Io(format!("UDP bind failed on 0.0.0.0:{port}: {e}"))`.
///
/// Cross-platform: stdlib maps `EADDRINUSE` (Linux/macOS) and `WSAEADDRINUSE`
/// (Windows, errno 10048) to `ErrorKind::AddrInUse`.
fn classify_bind_error(e: std::io::Error, port: u16) -> TransportError {
    if e.kind() == std::io::ErrorKind::AddrInUse {
        TransportError::AddrInUse { port }
    } else {
        TransportError::Io(format!("UDP bind failed on 0.0.0.0:{port}: {e}"))
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod qsv_ledger_slice2_tests {
    use super::validate_and_strip_qsv_ledger_offer;

    const OFFER: &str = "v=0\r\no=- 42 7 IN IP4 127.0.0.1\r\ns=screen-mirror\r\n";

    #[test]
    fn validated_marker_is_bound_to_exact_offer_session_and_stripped_before_str0m() {
        let offer = format!("{OFFER}a=x-sm-qsv-ledger:1:42%207:3\r\n");

        let accepted = validate_and_strip_qsv_ledger_offer(&offer, 3).unwrap();

        assert_eq!(
            accepted.marker,
            Some("a=x-sm-qsv-ledger:1:42%207:3".to_string())
        );
        assert_eq!(accepted.offer, OFFER);
    }

    #[test]
    fn invalid_or_stale_marker_is_rejected_without_changing_media_offer() {
        let stale = format!("{OFFER}a=x-sm-qsv-ledger:1:42%207:2\r\n");
        let wrong_session = format!("{OFFER}a=x-sm-qsv-ledger:1:99%207:3\r\n");

        assert!(validate_and_strip_qsv_ledger_offer(&stale, 3).is_err());
        assert!(validate_and_strip_qsv_ledger_offer(&wrong_session, 3).is_err());
    }

    #[test]
    fn missing_marker_fails_open_with_the_original_offer() {
        let accepted = validate_and_strip_qsv_ledger_offer(OFFER, 3).unwrap();

        assert_eq!(accepted.marker, None);
        assert_eq!(accepted.offer, OFFER);
    }
}

#[cfg(test)]
#[allow(clippy::type_complexity)]
mod tests {
    use std::sync::Arc;
    use std::sync::mpsc::sync_channel;

    use sm_domain::encode::EncodedPacket;
    use sm_domain::transport::{TransportConfig, TransportError, TransportEvent, VideoReceiver};

    use crate::diagnostics::qsv_ledger::TransportLedgerProbe;
    use crate::transport::str0m_receiver::{
        ReceiverControl, ReceiverLoopContext, ReceiverPreNeg, Str0mVideoReceiver, run_receiver_loop,
    };

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

    #[test]
    fn p1_receiver_loop_context_attached_probe_preserves_pointer_identity() {
        let receiver = Str0mVideoReceiver::new(TransportConfig {
            role: sm_domain::transport::TransportRole::Receiver,
            ..TransportConfig::default()
        })
        .expect("P1 receiver construction must succeed");
        let probe = Arc::new(TransportLedgerProbe::collecting());

        receiver.install_transport_ledger_probe_for_test(Arc::clone(&probe));
        let context: ReceiverLoopContext = receiver.receiver_loop_context();
        let context_probe = context
            .probe
            .expect("an attached probe must be propagated into the receiver loop context");

        assert!(Arc::ptr_eq(&probe, &context_probe));
    }

    #[test]
    fn p1_receiver_loop_context_unattached_probe_is_none_and_inert() {
        let receiver = Str0mVideoReceiver::new(TransportConfig {
            role: sm_domain::transport::TransportRole::Receiver,
            ..TransportConfig::default()
        })
        .expect("P1 receiver construction must succeed");

        let context: ReceiverLoopContext = receiver.receiver_loop_context();

        assert!(context.probe.is_none());
    }

    #[test]
    fn p1_receiver_loop_context_is_the_run_receiver_loop_handoff_argument() {
        let _: fn(
            ReceiverPreNeg,
            std::net::UdpSocket,
            std::net::SocketAddr,
            std::sync::mpsc::SyncSender<EncodedPacket>,
            std::sync::mpsc::SyncSender<TransportEvent>,
            std::sync::mpsc::Receiver<ReceiverControl>,
            ReceiverLoopContext,
        ) = run_receiver_loop;
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
        let result = receiver.apply_remote_offer_for_epoch(domain_offer, 1);
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
        let _ = receiver.apply_remote_offer_for_epoch(dummy_offer, 1);
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

    /// R6.3 — `apply_remote_offer` MUST work AFTER `start()` as well as before.
    ///
    /// RED: today this fails because `apply_remote_offer` returns
    /// `Err(Internal("Rtc already moved to thread; …"))` once `start()` has
    /// taken the `Rtc` out of `pre_neg`.  The dual-path inbox+reply
    /// implementation (design §3.2) will make this pass.
    #[test]
    fn str0m_receiver_apply_remote_offer_after_start_returns_answer_r6_3() {
        use std::time::Duration;

        use str0m::Rtc;
        use str0m::media::{Direction, MediaKind};

        // Build a minimal sender-side Rtc to generate a real SDP offer.
        let crypto_s = str0m::crypto::from_feature_flags();
        let mut rtc_sender = Rtc::builder()
            .set_crypto_provider(std::sync::Arc::new(crypto_s))
            .build(std::time::Instant::now());
        let mut change = rtc_sender.sdp_api();
        change.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
        let (str0m_offer, _pending) = change.apply().unwrap();
        let domain_offer = sm_domain::signaling::SdpOffer(str0m_offer.to_string());

        // Construct and START the receiver before calling apply_remote_offer.
        let mut receiver = Str0mVideoReceiver::new(TransportConfig {
            udp_port: 0,
            role: sm_domain::transport::TransportRole::Receiver,
            ..TransportConfig::default()
        })
        .unwrap();

        let (pkt_tx, _pkt_rx) = sync_channel::<EncodedPacket>(4);
        let (event_tx, _event_rx) = sync_channel::<TransportEvent>(4);
        receiver.start(pkt_tx, event_tx).unwrap();

        // Give the tick thread a moment to enter its first loop iteration.
        std::thread::sleep(Duration::from_millis(20));

        // Post-start call — MUST succeed via the inbox+reply path (design §3.2).
        let result = receiver.apply_remote_offer_for_epoch(domain_offer, 1);
        assert!(
            result.is_ok(),
            "apply_remote_offer AFTER start() must return Ok(SdpAnswer), got: {result:?}"
        );
        let answer = result.unwrap();
        assert!(
            answer.0.contains("v=0"),
            "SdpAnswer must be valid SDP containing 'v=0', got: {}",
            answer.0
        );

        receiver.stop().unwrap();
    }

    // ─── B2 RED: classify_bind_error logic unit test (R1.4) ───────────────────

    use super::classify_bind_error;

    #[test]
    fn classify_bind_error_addr_in_use_maps_to_transport_addr_in_use() {
        let io_err = std::io::Error::from(std::io::ErrorKind::AddrInUse);
        let result = classify_bind_error(io_err, 9876);
        match result {
            TransportError::AddrInUse { port: 9876 } => {}
            other => panic!("expected TransportError::AddrInUse {{ port: 9876 }}, got {other:?}"),
        }
    }

    #[test]
    fn classify_bind_error_other_error_maps_to_transport_io() {
        let io_err = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        let result = classify_bind_error(io_err, 9876);
        match result {
            TransportError::Io(msg) => {
                assert!(
                    msg.contains("UDP bind failed on"),
                    "Io message must contain 'UDP bind failed on', got: {msg}"
                );
            }
            other => panic!("expected TransportError::Io(_), got {other:?}"),
        }
    }

    // ─── B1 RED: start_with_socket — accepts externally-bound socket (R5.2, R5.3)

    /// B1-T1 — `start_with_socket` MUST accept an externally-bound `UdpSocket` and
    /// return `Ok(())` without attempting a second bind on the same address.
    ///
    /// RED until `start_with_socket` is implemented (B1.T2).
    #[test]
    fn start_with_socket_does_not_rebind() {
        use std::net::UdpSocket;
        let udp = UdpSocket::bind("0.0.0.0:0").expect("ephemeral bind must succeed");

        let mut receiver = Str0mVideoReceiver::new(TransportConfig {
            udp_port: 0,
            role: sm_domain::transport::TransportRole::Receiver,
            ..TransportConfig::default()
        })
        .unwrap();

        let (pkt_tx, _pkt_rx) = sync_channel::<EncodedPacket>(4);
        let (event_tx, _event_rx) = sync_channel::<sm_domain::transport::TransportEvent>(4);

        // If `start_with_socket` does not exist or calls bind internally,
        // this will fail to compile or panic with AddrInUse on the second bind.
        let result = receiver.start_with_socket(udp, pkt_tx, event_tx);
        assert!(
            result.is_ok(),
            "start_with_socket must return Ok(()) for a fresh prebound socket, got: {result:?}"
        );
        receiver.stop().unwrap();
    }

    // ─── WU-D1: first-media one-shot emit (media-arrival watchdog, #971 §D4) ───
    //
    // The receiver emits exactly ONE `TransportEvent::MediaData` per generation on
    // the first `str0m::Event::MediaData`, so the drain's post-rebuild watchdog can
    // disarm when media genuinely flows. The one-shot guard is a per-generation
    // `AtomicBool` (on `ReceiverShared`, reset to `false` by each `new()` — NOT a
    // static), so a fresh generation re-arms naturally.

    use super::emit_first_media_once;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    /// D1.1 — `emit_first_media_once` sends `TransportEvent::MediaData` on the FIRST
    /// call and is silent on every subsequent call (one-shot per generation). A new
    /// flag instance (new generation) emits again.
    ///
    /// RED: `emit_first_media_once` does not exist yet (compile failure).
    /// GREEN (WU-D1): compare-and-set guard + `try_send(MediaData)` on first media.
    #[test]
    fn sc_wd_media_data_emitted_on_first_event_media() {
        let (event_tx, event_rx) = sync_channel::<TransportEvent>(4);
        let media_emitted = AtomicBool::new(false);

        // First media → exactly one MediaData event.
        emit_first_media_once(&media_emitted, &event_tx);
        let first = event_rx.recv_timeout(Duration::from_millis(100));
        assert!(
            matches!(first, Ok(TransportEvent::MediaData)),
            "first media must emit exactly one TransportEvent::MediaData, got {first:?}"
        );

        // Second (and third) media on the SAME generation → no further emit.
        emit_first_media_once(&media_emitted, &event_tx);
        emit_first_media_once(&media_emitted, &event_tx);
        let none = event_rx.recv_timeout(Duration::from_millis(100));
        assert!(
            none.is_err(),
            "subsequent media on the same generation must NOT re-emit MediaData, got {none:?}"
        );

        // A NEW generation (fresh flag) re-arms and emits again — proves the guard
        // lives per-generation, not as a process-wide static.
        let media_emitted_gen2 = AtomicBool::new(false);
        emit_first_media_once(&media_emitted_gen2, &event_tx);
        let gen2 = event_rx.recv_timeout(Duration::from_millis(100));
        assert!(
            matches!(gen2, Ok(TransportEvent::MediaData)),
            "a fresh generation must re-emit MediaData on its first media, got {gen2:?}"
        );
    }

    // ─── GapStats unit tests (RECV-OBS-1, Slice 2) ───────────────────────────

    use super::GapStats;

    /// Three synthetic arrivals: record gaps [30ms, 80ms, 40ms] and assert
    /// max_gap_ms, mean_gap_ms, and receive_fps over a 1-second window.
    #[test]
    fn gap_stats_record_three_deltas() {
        let mut stats = GapStats::default();
        stats.record(Duration::from_millis(30));
        stats.record(Duration::from_millis(80));
        stats.record(Duration::from_millis(40));
        assert_eq!(stats.max_gap_ms(), 80.0, "max_gap_ms must be 80.0");
        // mean = (30000 + 80000 + 40000) µs / 3 / 1000 = 50.0 ms
        assert_eq!(stats.mean_gap_ms(), 50.0, "mean_gap_ms must be 50.0");
        let fps = stats.receive_fps(Duration::from_secs(1));
        assert!(
            (fps - 3.0).abs() < 1e-9,
            "receive_fps over 1s must be 3.0, got {fps}"
        );
    }

    /// Empty GapStats must return 0.0 for all metrics without panicking (div-by-zero guard).
    #[test]
    fn gap_stats_zero_guard() {
        let stats = GapStats::default();
        assert_eq!(stats.receive_fps(Duration::from_secs(1)), 0.0);
        assert_eq!(stats.mean_gap_ms(), 0.0);
        assert_eq!(stats.max_gap_ms(), 0.0);
    }

    /// After reset(), all fields must return 0.0.
    #[test]
    fn gap_stats_reset_zeroes() {
        let mut stats = GapStats::default();
        stats.record(Duration::from_millis(30));
        stats.record(Duration::from_millis(60));
        stats.reset();
        assert_eq!(stats.receive_fps(Duration::from_secs(1)), 0.0);
        assert_eq!(stats.mean_gap_ms(), 0.0);
        assert_eq!(stats.max_gap_ms(), 0.0);
    }
}
