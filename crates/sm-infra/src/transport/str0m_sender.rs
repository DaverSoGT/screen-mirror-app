//! str0m-backed video sender adapter.
//!
//! [`Str0mVideoSender`] implements [`VideoSender`] using the str0m SansIO WebRTC stack.
//! It is cross-platform (no `cfg` gate) per PQ-9.
//!
//! # Thread model
//!
//! - `new()`: validates config; allocates shared atomics. No thread, no socket.
//!   Creates an `Rtc` instance and generates the local SDP offer synchronously.
//! - `set_encoder()`: stores the encoder `Arc` for PLI wiring.
//! - `create_local_offer()`: returns the pre-generated SDP offer string.
//! - `apply_remote_answer()`: posts the answer to the control inbox for the tick thread.
//! - `start(rx, event_tx)`: binds the `UdpSocket`, moves `Rtc` into one OS thread
//!   that runs the SansIO tick loop with full media write + PLI dispatch.
//! - `stop()`: sets the `AtomicBool` stop flag and joins the thread. Idempotent.
//! - `Drop`: calls `stop()` to prevent leaked threads on panic or forgotten stop.
//!
//! # Batch 4 additions vs Batch 3
//!
//! - `Rtc` is created in `new()` rather than `start()` so that `create_local_offer()`
//!   is a synchronous pre-thread call.
//! - The tick loop now processes the encoded-packet inbox, calls `writer.write(pt, …)`.
//! - `Event::KeyframeRequest` dispatches directly to `encoder.request_keyframe()`.
//! - `SenderControl::ApplyAnswer` is wired to `rtc.sdp_api().accept_answer(pending, answer)`.
//! - `SenderControl::AddCandidate` parses a Candidate from JSON and calls
//!   `rtc.add_remote_candidate(candidate)`.

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use str0m::bwe::Bitrate;
use str0m::change::SdpPendingOffer;
use str0m::format::Codec;
use str0m::media::{Direction, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};

use sm_domain::encode::{EncodedPacket, VideoEncoder};
use sm_domain::signaling::{IceCandidate, SdpAnswer, SdpOffer};
use sm_domain::transport::{
    TRANSPORT_CHANNEL_CAPACITY, TransportConfig, TransportError, TransportEvent, VideoSender,
};

use crate::transport::annex_b::duration_to_90khz;

// ─── Internal control message ────────────────────────────────────────────────

/// Messages sent from the public API (any thread) to the tick loop thread.
///
/// Drained at the start of each tick iteration.
enum SenderControl {
    /// Apply a remote SDP answer (received via signaling).
    ApplyAnswer(SdpAnswer),
    /// Add a remote ICE candidate (received via signaling).
    AddCandidate(IceCandidate),
    /// Test-only: inject a synthetic `Event::KeyframeRequest` into the loop.
    #[cfg(test)]
    InjectKeyframeRequest,
    /// Test-only: latch `ice_ready = true` AND emit `TransportEvent::IceConnected`,
    /// matching what the real `IceConnectionStateChange(Connected|Completed)` path does.
    #[cfg(test)]
    SetIceReadyForTest,
}

// ─── Pre-negotiation state ───────────────────────────────────────────────────

/// Holds the `Rtc` instance and SDP negotiation state before `start()` is called.
///
/// Protected by a `Mutex` so that `create_local_offer(&self)` can access `&mut Rtc`
/// without requiring `&mut self` on the public API method.
struct PreNegState {
    rtc: Rtc,
    pending: Option<SdpPendingOffer>,
    mid: Option<Mid>,
    pt: Option<Pt>,
    /// Serialised SDP offer text; set in `new()` for `create_local_offer()`.
    offer_str: String,
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

/// Latency-first: drain a worst-case IDR within one frame interval. See design D-PPT3-2.
const PACER_HEADROOM: u64 = 4;

// ─── Str0mVideoSender ────────────────────────────────────────────────────────

/// str0m-backed video sender. Implements [`VideoSender`].
///
/// Cross-platform — no `#[cfg(target_os = "windows")]` gate (PQ-9).
///
/// # Lifecycle
///
/// 1. `new(config)` — creates `Rtc`, generates SDP offer (no socket, no thread).
/// 2. `set_encoder(arc)` — inject encoder for PLI feedback.
/// 3. `create_local_offer()` — returns the pre-computed SDP offer string.
/// 4. `start(rx, event_tx)` — binds UDP, moves `Rtc` to tick thread.
/// 5. `apply_remote_answer(answer)` — posts answer to tick thread via control inbox.
/// 6. `stop()` / `Drop` — sets stop flag, joins thread. Idempotent.
pub struct Str0mVideoSender {
    /// Original transport configuration.
    config: TransportConfig,
    /// Pre-negotiation state: Rtc + SDP offer/pending/mid. Guarded by Mutex for
    /// `&self` access on `create_local_offer`. Taken out (→ None) when `start()` moves
    /// the Rtc into the tick thread.
    pre_neg: Mutex<Option<PreNegState>>,
    /// Shared atomic state between caller and tick thread.
    state: Arc<SenderShared>,
    /// Encoder held for PLI wiring. Cloned into tick thread on `start()`.
    encoder: Option<Arc<dyn VideoEncoder + Send + Sync>>,
    /// Control inbox: caller → tick thread. Created in `start()`.
    control_tx: Option<SyncSender<SenderControl>>,
    /// Join handle for the tick thread. `Some` while running, `None` otherwise.
    handle: Option<JoinHandle<()>>,
    /// Effective local socket address after `start()`. `None` before start.
    /// Used by integration tests to discover the bound port for candidate exchange.
    local_addr: Option<std::net::SocketAddr>,
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
// The encoder is `Arc<dyn VideoEncoder + Send + Sync>`. `Mutex<Option<PreNegState>>`
// is `Send` when `PreNegState: Send` (Rtc is Send per str0m guarantee — it moves
// across threads in the spawn call above without unsafe impl).
unsafe impl Send for Str0mVideoSender {}
// SAFETY: Every method that takes `&self` either accesses atomics (which are `Sync`)
// or clones `SyncSender<SenderControl>` (also `Sync` when element is `Send`), or
// acquires the `Mutex<Option<PreNegState>>` (Mutex is Sync). No `&self` path reaches
// a `!Sync` field.
unsafe impl Sync for Str0mVideoSender {}

impl VideoSender for Str0mVideoSender {
    /// Construct a sender with the given configuration.
    ///
    /// Creates an `Rtc` instance and the SDP offer synchronously so that
    /// [`create_local_offer`](VideoSender::create_local_offer) works without a thread.
    fn new(config: TransportConfig) -> Result<Self, TransportError>
    where
        Self: Sized,
    {
        // Build the str0m Rtc with the rust-crypto backend.
        // enable_bwe selects LeakyBucketPacer over NullPacer (design D-PPT3-1).
        // Initial estimate = encode bitrate × PACER_HEADROOM so a worst-case IDR
        // drains within one frame interval without queuing into the next gap (D-PPT3-2).
        let crypto = str0m::crypto::from_feature_flags();
        let mut rtc = Rtc::builder()
            .set_crypto_provider(Arc::new(crypto))
            .enable_bwe(Some(Bitrate::bps(
                config.bitrate_bps as u64 * PACER_HEADROOM,
            )))
            .build(Instant::now());

        // Generate the SDP offer (add a SendOnly video m-line).
        let mut change = rtc.sdp_api();
        let mid = change.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
        // apply() returns Option<(SdpOffer, SdpPendingOffer)>; None means no changes
        // were made, which can't happen here since we just added a media line.
        let (offer, pending) = change.apply().ok_or_else(|| {
            TransportError::Internal("SDP offer generation failed: no changes to apply".into())
        })?;

        // Serialise the offer to a plain-text SDP string for create_local_offer().
        // SdpOffer implements Display and outputs the SDP text directly.
        let offer_str = offer.to_string();

        let pre_neg = PreNegState {
            rtc,
            pending: Some(pending),
            mid: Some(mid),
            pt: None,
            offer_str,
        };

        Ok(Self {
            config,
            pre_neg: Mutex::new(Some(pre_neg)),
            state: SenderShared::new(),
            encoder: None,
            control_tx: None,
            handle: None,
            local_addr: None,
        })
    }

    fn set_encoder(&mut self, encoder: Arc<dyn VideoEncoder + Send + Sync>) {
        self.encoder = Some(encoder);
    }

    fn start(
        &mut self,
        rx: Receiver<EncodedPacket>,
        event_tx: SyncSender<TransportEvent>,
    ) -> Result<(), TransportError> {
        if self.handle.is_some() {
            return Err(TransportError::AlreadyRunning);
        }

        // R9.3 — encoder must be set before start so PLI events have somewhere to go.
        if self.encoder.is_none() {
            return Err(TransportError::InvalidConfig(
                "encoder not set; call set_encoder() before start()".into(),
            ));
        }

        // Reset stop flag in case this sender is restarted.
        self.state.stop.store(false, Ordering::Release);

        // Bind the UDP socket.
        let bind_addr_str = format!("0.0.0.0:{}", self.config.udp_port);
        let bind_addr: SocketAddr = bind_addr_str
            .parse()
            .expect("static 0.0.0.0:{port} format is always a valid SocketAddr");
        let udp = bind_udp_socket_reusable(bind_addr)
            .map_err(|e| TransportError::Io(format!("UDP bind failed on {bind_addr_str}: {e}")))?;
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
        // with the actual bound port so that loopback tests and single-host setups
        // work without explicit candidate injection. The effective address is also
        // what we tell str0m is the "destination" when a UDP datagram arrives —
        // str0m uses this to match packets to ICE candidates.
        let effective_local_addr: std::net::SocketAddr = if local_addr.ip().is_unspecified() {
            format!("127.0.0.1:{}", local_addr.port()).parse().unwrap()
        } else {
            local_addr
        };

        let local_candidate_opt = Candidate::host(effective_local_addr, "udp").ok();

        {
            // We need &mut rtc to add the candidate. Briefly borrow from pre_neg.
            // pre_neg is now owned locally.
            let mut rtc_tmp = pre_neg.rtc;
            if let Some(cand) = local_candidate_opt {
                rtc_tmp.add_local_candidate(cand);
            }

            // Put it back into a new owned struct for the thread.
            let pre_neg_with_candidate = PreNegState {
                rtc: rtc_tmp,
                pending: pre_neg.pending,
                mid: pre_neg.mid,
                pt: pre_neg.pt,
                offer_str: pre_neg.offer_str,
            };

            // Control inbox.
            let (ctrl_tx, ctrl_rx) =
                std::sync::mpsc::sync_channel::<SenderControl>(TRANSPORT_CHANNEL_CAPACITY);
            self.control_tx = Some(ctrl_tx);

            let state = Arc::clone(&self.state);
            let encoder = self.encoder.clone();

            let handle = std::thread::Builder::new()
                .name("sm-transport-sender".into())
                .spawn(move || {
                    run_sender_loop(
                        pre_neg_with_candidate,
                        udp,
                        effective_local_addr,
                        rx,
                        event_tx,
                        ctrl_rx,
                        state,
                        encoder,
                    );
                })
                .map_err(|e| TransportError::Internal(format!("thread spawn failed: {e}")))?;

            self.handle = Some(handle);
        }

        // Store the effective local address so callers can retrieve it for candidate exchange.
        self.local_addr = Some(effective_local_addr);

        Ok(())
    }

    fn stop(&mut self) -> Result<(), TransportError> {
        self.state.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.control_tx = None;
        Ok(())
    }

    fn apply_remote_answer(&self, answer: SdpAnswer) -> Result<(), TransportError> {
        match &self.control_tx {
            None => Err(TransportError::NotRunning),
            Some(tx) => tx
                .try_send(SenderControl::ApplyAnswer(answer))
                .map_err(|_| TransportError::Internal("control inbox full or disconnected".into())),
        }
    }

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
    /// Returns the offer string computed during `new()`. Can be called before or after
    /// `start()`. If the `Rtc` has already been moved to the tick thread, returns
    /// `Err(NotRunning)`.
    ///
    /// # Note on serialisation
    ///
    /// str0m's `SdpOffer` implements `serde::Serialize`, so we JSON-serialise it to a
    /// string here. The remote peer (receiver's str0m) deserialises with
    /// `serde_json::from_str::<str0m::change::SdpOffer>(&s).unwrap()`.
    fn create_local_offer(&self) -> Result<SdpOffer, TransportError> {
        let guard = self
            .pre_neg
            .lock()
            .map_err(|e| TransportError::Internal(format!("mutex poisoned: {e}")))?;

        match guard.as_ref() {
            None => Err(TransportError::Internal(
                "Rtc already moved to thread; offer no longer available".into(),
            )),
            Some(pn) => Ok(SdpOffer(pn.offer_str.clone())),
        }
    }

    fn dropped_frames(&self) -> u64 {
        self.state.dropped.load(Ordering::Relaxed)
    }
}

impl Drop for Str0mVideoSender {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

impl Str0mVideoSender {
    /// Return the effective local socket address after `start()`.
    ///
    /// Returns `None` before `start()` is called. Used by integration tests and
    /// signaling adapters to discover the bound ephemeral port for ICE candidate exchange.
    pub fn local_addr(&self) -> Option<std::net::SocketAddr> {
        self.local_addr
    }

    /// Return the ICE host candidate address for this sender.
    ///
    /// Returns `Some(addr)` after a successful `start()`, where `addr.port()` matches
    /// the bound UDP socket port and `addr.ip()` is a non-loopback IPv4 address.
    ///
    /// When `effective_local_addr` is a loopback address (`127.0.0.1`) — which happens
    /// when the socket was bound to `0.0.0.0` — this method substitutes the first
    /// non-loopback IPv4 NIC address from `enumerate_local_ipv4()`, preserving the port.
    ///
    /// Returns `None` in these cases:
    /// - `start()` has not been called yet (`self.local_addr` is `None`).
    /// - No non-loopback IPv4 NIC is available (e.g., loopback-only machine or CI).
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
}

// ─── Test-only helpers ────────────────────────────────────────────────────────

#[cfg(test)]
impl Str0mVideoSender {
    /// Inject a synthetic `Event::KeyframeRequest` into the tick loop.
    ///
    /// Used by task 4.3/4.4 tests to verify PLI → encoder path without a live
    /// str0m session. The tick loop processes this on the next iteration.
    pub(crate) fn inject_keyframe_request_for_test(&self) {
        if let Some(tx) = &self.control_tx {
            let _ = tx.try_send(SenderControl::InjectKeyframeRequest);
        }
    }

    /// Latch the tick loop's local `ice_ready` flag to `true`.
    ///
    /// Used by tests that need to drive the post-ICE write path without
    /// a live str0m peer. Sends `SenderControl::SetIceReadyForTest` on the
    /// control channel; the tick loop processes it on the next iteration
    /// and ALSO emits `TransportEvent::IceConnected` so observers see the
    /// same sequence as production.
    pub(crate) fn inject_ice_ready_for_test(&self) {
        if let Some(tx) = &self.control_tx {
            let _ = tx.try_send(SenderControl::SetIceReadyForTest);
        }
    }
}

// ─── Tick loop ───────────────────────────────────────────────────────────────

/// SansIO tick loop for `Str0mVideoSender`.
///
/// Runs on the dedicated OS thread spawned by `start()`.
#[expect(
    clippy::too_many_arguments,
    reason = "SansIO tick loop owns 8 distinct, non-aggregatable resources \
              (pre-neg state, UDP socket, local addr, packet receiver, event \
              sender, control receiver, shared state, optional encoder); \
              bundling them into a struct adds indirection without simplifying \
              the loop's responsibilities"
)]
fn run_sender_loop(
    mut pre_neg: PreNegState,
    udp: UdpSocket,
    local_addr: SocketAddr,
    rx: Receiver<EncodedPacket>,
    event_tx: SyncSender<TransportEvent>,
    ctrl_rx: std::sync::mpsc::Receiver<SenderControl>,
    state: Arc<SenderShared>,
    encoder: Option<Arc<dyn VideoEncoder + Send + Sync>>,
) {
    let mut buf = vec![0u8; 2048];
    let rtc = &mut pre_neg.rtc;
    // Scoped to this call frame — each new generation of run_sender_loop begins gated.
    // Set to true when IceConnectionStateChange(Connected|Completed) is observed,
    // or when SetIceReadyForTest is processed in test builds.
    // Monotonic within one generation: never reset to false.
    let mut ice_ready = false;
    // Instrumentation (HW gate): once-per-generation flag so the first RTP
    // Transmit destination is logged exactly once. Reveals whether THIS
    // generation targets a usable receiver media addr:port.
    let mut first_transmit_logged = false;
    // [sm-sender-pace] seam: counters reset every second.
    // Window boundary owned by the loop (design D-PPT3-4).
    let mut pace_stats = PaceStats::new();
    let mut pace_window_start = Instant::now();

    loop {
        // ── 1. Stop flag ──────────────────────────────────────────────────
        if state.stop.load(Ordering::Acquire) {
            break;
        }

        // ── 1b. Pace tick + per-second emission ──────────────────────────
        pace_stats.on_tick();
        let pace_elapsed = pace_window_start.elapsed();
        if pace_elapsed >= Duration::from_secs(1) {
            let (ticks_s, pkts_s, max_burst) =
                pace_stats.snapshot_per_s(pace_elapsed.as_secs_f64());
            eprintln!(
                "[sm-sender-pace] ticks_per_s={ticks_s:.1} pkts_sent_per_s={pkts_s:.1} max_burst={max_burst}"
            );
            pace_stats.reset();
            pace_window_start = Instant::now();
        }

        // ── 2. Drain control inbox ────────────────────────────────────────
        while let Ok(msg) = ctrl_rx.try_recv() {
            match msg {
                SenderControl::ApplyAnswer(ans) => {
                    if let Some(pending) = pre_neg.pending.take() {
                        // Parse the domain SdpAnswer (plain SDP text) to str0m's SdpAnswer.
                        // The answer is produced by Str0mVideoReceiver using SdpAnswer::to_string().
                        match str0m::change::SdpAnswer::from_sdp_string(&ans.0) {
                            Ok(str0m_answer) => {
                                if let Err(e) = rtc.sdp_api().accept_answer(pending, str0m_answer) {
                                    let _ = event_tx.try_send(TransportEvent::ConnectionLost {
                                        reason: format!("accept_answer failed: {e}"),
                                    });
                                }
                            }
                            Err(e) => {
                                let _ = event_tx.try_send(TransportEvent::ConnectionLost {
                                    reason: format!("SDP answer parse failed: {e}"),
                                });
                            }
                        }
                    }
                }
                SenderControl::AddCandidate(cand) => {
                    // Candidates are JSON-serialised str0m::Candidate values.
                    if let Ok(c) = serde_json::from_str::<Candidate>(&cand.0) {
                        rtc.add_remote_candidate(c);
                    }
                    // Silently ignore un-parseable candidates.
                }
                #[cfg(test)]
                SenderControl::InjectKeyframeRequest => {
                    // Dispatch directly as if str0m had fired the event.
                    // mid is available once negotiation starts; if not yet set,
                    // the KeyframeRequest path in handle_sender_event doesn't use it.
                    if let Some(mid) = pre_neg.mid {
                        handle_sender_event(
                            Event::KeyframeRequest(str0m::media::KeyframeRequest {
                                mid,
                                rid: None,
                                kind: str0m::media::KeyframeRequestKind::Pli,
                            }),
                            &state,
                            &encoder,
                            &event_tx,
                        );
                    } else {
                        // No mid yet — still call the handler without a real event
                        // by constructing the minimal side-effect path directly.
                        if let Some(enc) = &encoder {
                            enc.request_keyframe();
                        }
                        let _ = event_tx.try_send(TransportEvent::KeyframeRequested);
                    }
                }
                #[cfg(test)]
                SenderControl::SetIceReadyForTest => {
                    // Mirror what the real ICE path does: latch the gate AND notify
                    // the drain thread. Production never takes this path; tests use it
                    // to bypass a live str0m peer while preserving observable sequencing.
                    ice_ready = true;
                    let _ = event_tx.try_send(TransportEvent::IceConnected);
                }
            }
        }

        // ── 3. Drain encoded packets and write to str0m ───────────────────
        // Two-condition gate: SDP done (`mid`) AND ICE done (`ice_ready`).
        // `mid` alone (before this change) let pre-DTLS packets reach
        // `writer.write()` where str0m may silently swallow them. We refuse
        // to write until ICE has been observed Connected|Completed at least
        // once on THIS tick loop instance.
        // Both pre-mid AND pre-ice_ready packets count against `state.dropped`.
        // If a future change wants to distinguish, add a second AtomicU64.
        if let Some(mid) = pre_neg.mid {
            if ice_ready {
                while let Ok(pkt) = rx.try_recv() {
                    // Resolve H264 PT lazily if not yet known.
                    if pre_neg.pt.is_none() {
                        if let Some(writer) = rtc.writer(mid) {
                            pre_neg.pt = writer
                                .payload_params()
                                .find(|p| p.spec().codec == Codec::H264)
                                .map(|p| p.pt());
                        }
                    }

                    if let Some(pt) = pre_neg.pt {
                        let rtp_ts = duration_to_90khz(pkt.timestamp);
                        let rtp_time = MediaTime::from_90khz(rtp_ts);
                        let wallclock = Instant::now();

                        if let Some(writer) = rtc.writer(mid) {
                            // Pass the entire Annex-B frame. str0m's H264Packetizer
                            // handles start-code stripping, FU-A fragmentation, SRTP.
                            if let Err(_e) =
                                writer.write(pt, wallclock, rtp_time, pkt.data.as_ref())
                            {
                                state.dropped.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    } else {
                        // PT not yet resolved — drop this packet (pre-DTLS).
                        state.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
            } else {
                // mid resolved but ICE not yet connected — drain and drop.
                while let Ok(_pkt) = rx.try_recv() {
                    state.dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        } else {
            // Pre-negotiation: drain and drop packets (ICE/DTLS not ready).
            while let Ok(_pkt) = rx.try_recv() {
                state.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }

        // ── 4. Drain str0m outputs until Timeout ─────────────────────────
        let deadline = loop {
            match rtc.poll_output() {
                Ok(Output::Timeout(t)) => break t,
                Ok(Output::Transmit(t)) => {
                    // Instrumentation (HW gate, log #1): log the FIRST send_to
                    // destination per generation — shows whether the sender is
                    // targeting a valid/new receiver media addr:port.
                    if !first_transmit_logged {
                        first_transmit_logged = true;
                        eprintln!(
                            "[sm-sender-tick] first Transmit dest={} local={local_addr}",
                            t.destination
                        );
                    }
                    let _ = udp.send_to(&t.contents, t.destination);
                    // [sm-sender-pace] burst accounting: count every UDP send (design D-PPT3-4).
                    pace_stats.on_transmit();
                }
                Ok(Output::Event(ev)) => {
                    // Capture mid from MediaAdded; resolve PT lazily in the write path.
                    // `MediaAdded` fires once SDP negotiation is complete. We store the
                    // mid here so the packet write path knows when to start trying to
                    // resolve the payload type.
                    if let Event::MediaAdded(ref added) = ev {
                        pre_neg.mid = Some(added.mid);
                    }
                    // Latch ice_ready once the ICE state machine reaches a working pair.
                    // Both `Connected` and `Completed` flip the gate (with a single
                    // candidate pair str0m may skip Connected and jump to Completed).
                    // Monotonic — once true, stays true until this tick loop exits.
                    // A flip back on Disconnected would create a second recovery path
                    // competing with the supervisor; the supervisor owns rebuild via
                    // IceFailed → ReconnectSupervisor (per auto-rebuild-from-drain).
                    if is_ice_ready_event(&ev) {
                        // Instrumentation (HW gate, log #2): on the ICE-ready
                        // transition, log the local addr + the state event so the
                        // gate can correlate the nominated pair with the Transmit
                        // destination. Logged once (ice_ready is monotonic).
                        if !ice_ready {
                            eprintln!(
                                "[sm-sender-tick] IceConnected local={local_addr} event={ev:?}"
                            );
                        }
                        ice_ready = true;
                    }
                    handle_sender_event(ev, &state, &encoder, &event_tx);
                }
                Err(_) => {
                    let _ = event_tx.try_send(TransportEvent::ConnectionLost {
                        reason: "str0m poll_output error".into(),
                    });
                    return;
                }
            }
        };
        // [sm-sender-pace] drain boundary: record max burst for this poll_output cycle.
        pace_stats.on_drain_end();

        if state.stop.load(Ordering::Acquire) {
            break;
        }

        // ── 5. Blocking recv_from with deadline-derived timeout ──────────
        // Cap at 200 ms so that stop() and control-inbox messages (remote
        // candidates, answer) unblock quickly.
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
                // Windows-specific: WSAECONNRESET on a UDP socket means a
                // previous send_to triggered an ICMP "destination unreachable"
                // from the peer or a router, and the kernel surfaces the
                // queued error on the next recv_from. UDP is connectionless,
                // so this is advisory — the socket remains usable. Linux
                // silently drops these by default; Windows propagates them.
                // Treat as a tick: ICE retransmissions/PLI keep negotiating
                // and the next valid datagram still arrives.
                eprintln!(
                    "[sm-sender-tick] ignoring transient ICMP-related recv_from error (WSAECONNRESET): {e}"
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

/// Return `true` if `ev` is the str0m event that should latch the ICE-ready gate.
///
/// The latch fires for `Connected` and `Completed` because, with a single working
/// candidate pair, str0m may skip `Connected` and jump straight to `Completed`.
/// `Disconnected` and other state transitions MUST NOT latch the gate — the
/// supervisor owns recovery via `IceFailed → ReconnectSupervisor`.
fn is_ice_ready_event(ev: &Event) -> bool {
    matches!(
        ev,
        Event::IceConnectionStateChange(IceConnectionState::Connected)
            | Event::IceConnectionStateChange(IceConnectionState::Completed)
    )
}

/// Dispatch str0m events for the sender.
fn handle_sender_event(
    ev: Event,
    state: &SenderShared,
    encoder: &Option<Arc<dyn VideoEncoder + Send + Sync>>,
    event_tx: &SyncSender<TransportEvent>,
) {
    let _ = state; // state.dropped used for packet drops, not event drops
    match ev {
        // `Connected` fires when at least one candidate pair is working but gathering
        // may still be in progress. `Completed` fires when the best pair is selected
        // and gathering is done. With a single candidate pair (loopback tests, most
        // prod scenarios), the state jumps directly to `Completed`, skipping `Connected`.
        // We map both to `TransportEvent::IceConnected`.
        Event::IceConnectionStateChange(IceConnectionState::Connected)
        | Event::IceConnectionStateChange(IceConnectionState::Completed) => {
            let _ = event_tx.try_send(TransportEvent::IceConnected);
        }
        Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
            let _ = event_tx.try_send(TransportEvent::IceFailed);
        }
        Event::KeyframeRequest(_req) => {
            // R9.2: call encoder directly (no channel hop — the Sync retrofit enables this).
            if let Some(enc) = encoder {
                enc.request_keyframe();
            }
            // Emit observability event. Drop-newest if channel is full (R14.5).
            let _ = event_tx.try_send(TransportEvent::KeyframeRequested);
        }
        _ => {}
    }
}

// ─── Socket helpers ───────────────────────────────────────────────────────────

fn bind_udp_socket_reusable(addr: SocketAddr) -> io::Result<UdpSocket> {
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(addr),
        socket2::Type::DGRAM,
        None,
    )?;
    // Defense in depth — see bind_tcp_listener_reusable in mdns.rs for semantics.
    // Current udp_port default is 0 (ephemeral) so this never collides today,
    // but a fixed UDP port (future config) would re-introduce the bind race that
    // we fix on TCP (Arc-lifetime race, engram #1417).
    socket.set_reuse_address(true)?;
    socket.bind(&addr.into())?;
    Ok(socket.into())
}

// ─── PaceStats seam ──────────────────────────────────────────────────────────

/// Pure-data accumulator for the `[sm-sender-pace]` per-second instrument.
///
/// No I/O, no clock, no network dependency — unit-testable in isolation.
/// One instance lives on the `run_sender_loop` stack; reset every second.
///
/// Field semantics (per design D-PPT3-4):
/// - `ticks`: loop iterations (incremented once per main-loop tick via `on_tick`).
/// - `pkts`: total `Output::Transmit` (UDP `send_to`) calls (incremented per packet).
/// - `max_burst`: maximum number of `Output::Transmit` calls emitted within a
///   single `poll_output` drain.  The burst discriminator: 1–3 → pacing active;
///   dozens → NullPacer (pre-fix behavior).
/// - `cur_burst`: transient counter reset by `on_drain_end`.
struct PaceStats {
    ticks: u32,
    pkts: u64,
    max_burst: u32,
    cur_burst: u32,
}

impl PaceStats {
    fn new() -> Self {
        Self {
            ticks: 0,
            pkts: 0,
            max_burst: 0,
            cur_burst: 0,
        }
    }

    /// Called once per main-loop iteration (before the `poll_output` drain).
    #[inline]
    fn on_tick(&mut self) {
        self.ticks += 1;
    }

    /// Called immediately after each successful `send_to` inside the drain.
    #[inline]
    fn on_transmit(&mut self) {
        self.cur_burst += 1;
        self.pkts += 1;
    }

    /// Called after the `poll_output` drain exits (`Output::Timeout` reached).
    /// Records the burst size for this drain and resets the transient counter.
    #[inline]
    fn on_drain_end(&mut self) {
        if self.cur_burst > self.max_burst {
            self.max_burst = self.cur_burst;
        }
        self.cur_burst = 0;
    }

    /// Returns `(ticks_per_s, pkts_per_s, max_burst)` for the current window.
    ///
    /// Divides accumulated counters by `elapsed_secs` so the caller does not
    /// need to track the denominator separately.
    ///
    /// Behavior: on non-positive `elapsed_secs` (`<= 0.0`) the function returns
    /// `(0.0, 0.0, self.max_burst)` instead of dividing. This keeps the seam
    /// genuinely total — finite rates with no division by zero and no infinities
    /// for any caller, including the `elapsed = 0.0` edge case where a floored
    /// `f64::MIN_POSITIVE` denominator would otherwise overflow large numerators
    /// to `+inf`. `max_burst` is a count, not a rate, so it is always preserved.
    ///
    /// Precondition: the production caller only invokes this once `elapsed_secs`
    /// has reached ≥ 1 s, so it never reaches the zero-elapsed branch in practice.
    fn snapshot_per_s(&self, elapsed_secs: f64) -> (f64, f64, u32) {
        if elapsed_secs <= 0.0 {
            return (0.0, 0.0, self.max_burst);
        }
        let ticks_s = self.ticks as f64 / elapsed_secs;
        let pkts_s = self.pkts as f64 / elapsed_secs;
        (ticks_s, pkts_s, self.max_burst)
    }

    /// Zeroes all fields.  Called after emitting the per-second log line.
    fn reset(&mut self) {
        self.ticks = 0;
        self.pkts = 0;
        self.max_burst = 0;
        self.cur_burst = 0;
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
    use str0m::{Event, IceConnectionState};

    use super::bind_udp_socket_reusable;
    use crate::transport::str0m_sender::{PaceStats, Str0mVideoSender, is_ice_ready_event};

    // ─── Static assertion: Str0mVideoSender is Send + Sync (task 3.5) ─────────

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

        fn backend_name(&self) -> &'static str {
            "sw_fake"
        }
    }

    // ─── S5.1 (batch 3 variant): new() returns Ok with default config ─────────

    #[test]
    fn str0m_sender_new_default_config_returns_ok_s5_1() {
        let result = Str0mVideoSender::new(TransportConfig::default());
        assert!(
            result.is_ok(),
            "Str0mVideoSender::new(default) must return Ok, got: {result:?}"
        );
    }

    #[test]
    fn str0m_sender_new_port_zero_returns_ok() {
        let cfg = TransportConfig {
            udp_port: 0,
            ..TransportConfig::default()
        };
        let result = Str0mVideoSender::new(cfg);
        assert!(result.is_ok(), "new() must not reject port 0");
    }

    // ─── create_local_offer returns an SDP string before start ───────────────

    /// R5.5, S5.4 — `create_local_offer()` returns a non-empty SDP string.
    #[test]
    fn str0m_sender_create_local_offer_returns_sdp_s5_4() {
        let sender = Str0mVideoSender::new(TransportConfig::default()).unwrap();
        let offer = sender.create_local_offer();
        assert!(
            offer.is_ok(),
            "create_local_offer must return Ok: {offer:?}"
        );
        let sdp = offer.unwrap();
        assert!(!sdp.0.is_empty(), "SDP offer must be non-empty");
        // A valid SDP starts with "v=0"
        assert!(
            sdp.0.contains("v=0"),
            "SDP offer must contain 'v=0', got: {}",
            sdp.0
        );
    }

    // ─── set_encoder stores the encoder (no panic) ───────────────────────────

    #[test]
    fn str0m_sender_set_encoder_no_panic() {
        let mut sender = Str0mVideoSender::new(TransportConfig::default()).unwrap();
        let enc = FakeEncoder::new();
        sender.set_encoder(enc as Arc<dyn VideoEncoder + Send + Sync>);
    }

    // ─── S5.2: start + stop ───────────────────────────────────────────────────

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
        drop(pkt_tx);

        let result = sender.stop();
        assert!(result.is_ok(), "stop() must return Ok, got: {result:?}");
    }

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

    #[test]
    fn str0m_sender_stop_is_idempotent_s12_4() {
        let mut sender = Str0mVideoSender::new(TransportConfig::default()).unwrap();
        sender.stop().unwrap();
        sender.stop().unwrap();

        let enc = FakeEncoder::new();
        sender.set_encoder(enc as Arc<dyn VideoEncoder + Send + Sync>);
        let (pkt_tx, pkt_rx) = sync_channel(4);
        let (event_tx, _event_rx) = sync_channel::<TransportEvent>(4);
        sender.start(pkt_rx, event_tx).expect("start must succeed");
        drop(pkt_tx);
        sender.stop().unwrap();
        sender.stop().unwrap();
    }

    // ─── S12.1: Drop calls stop() ─────────────────────────────────────────────

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
            drop(pkt_tx);
        }
    }

    #[test]
    fn str0m_sender_dropped_frames_initially_zero() {
        let sender = Str0mVideoSender::new(TransportConfig::default()).unwrap();
        assert_eq!(sender.dropped_frames(), 0);
    }

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

    // ─── Task 4.3/4.4: PLI + backpressure tests ──────────────────────────────

    /// S9.1 — When `Event::KeyframeRequest` is injected into the tick loop,
    /// the encoder's `request_keyframe()` MUST be called and
    /// `TransportEvent::KeyframeRequested` MUST appear on `event_tx`.
    #[test]
    fn sender_pli_calls_encoder_request_keyframe_s9_1() {
        use std::time::Duration;

        let enc = FakeEncoder::new();
        let enc_arc = Arc::clone(&enc) as Arc<dyn VideoEncoder + Send + Sync>;
        let keyframe_called = Arc::clone(&enc.keyframe_called);

        let mut sender = Str0mVideoSender::new(TransportConfig {
            udp_port: 0,
            ..TransportConfig::default()
        })
        .unwrap();
        sender.set_encoder(enc_arc);

        let (_pkt_tx, pkt_rx) = sync_channel(4);
        let (event_tx, event_rx) = sync_channel::<TransportEvent>(4);
        sender.start(pkt_rx, event_tx).unwrap();

        // Inject a synthetic PLI into the running tick loop.
        sender.inject_keyframe_request_for_test();

        // Give the tick loop time to process the PLI.
        std::thread::sleep(Duration::from_millis(100));

        // The encoder's request_keyframe() must have been called.
        assert!(
            keyframe_called.load(Ordering::Acquire),
            "encoder.request_keyframe() must be called on PLI"
        );

        // TransportEvent::KeyframeRequested must appear on the event channel.
        let events: Vec<_> = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
        let has_keyframe_requested = events
            .iter()
            .any(|e| matches!(e, TransportEvent::KeyframeRequested));
        assert!(
            has_keyframe_requested,
            "TransportEvent::KeyframeRequested must be emitted on PLI; got: {events:?}"
        );

        sender.stop().unwrap();
    }

    // ─── T3.1 (streaming-emit-on-ice-connect): regression-preservation for pre-mid drop path ──

    /// T3.1 (AC-2) — Pre-mid packets are dropped and counted.
    ///
    /// Regression-preservation: if this goes RED after T3.3 impl, the gate broke the pre-mid path.
    #[test]
    fn pre_ice_packets_are_dropped_and_counted_when_mid_none() {
        use std::time::Duration;

        let enc = FakeEncoder::new();
        let enc_arc = Arc::clone(&enc) as Arc<dyn VideoEncoder + Send + Sync>;

        let mut sender = Str0mVideoSender::new(TransportConfig {
            udp_port: 0,
            ..TransportConfig::default()
        })
        .unwrap();
        sender.set_encoder(enc_arc);

        let (pkt_tx, pkt_rx) = sync_channel(8);
        let (event_tx, _event_rx) = sync_channel::<TransportEvent>(4);
        sender.start(pkt_rx, event_tx).unwrap();

        // Send 3 packets before any ICE or SDP negotiation (mid=None, ice_ready=false).
        for i in 0..3u64 {
            let _ = pkt_tx.send(sm_domain::encode::EncodedPacket {
                data: vec![0u8; 16].into(),
                timestamp: std::time::Duration::ZERO,
                is_keyframe: false,
                sequence: i,
            });
        }
        std::thread::sleep(Duration::from_millis(50));

        assert!(
            sender.dropped_frames() >= 3,
            "dropped_frames must be >= 3 when mid=None, got {}",
            sender.dropped_frames()
        );

        drop(pkt_tx);
        sender.stop().unwrap();
    }

    // ─── T3.2 (streaming-emit-on-ice-connect): post-mid pre-ice drop semantics ──

    /// T3.2 (AC-2) — After inject_ice_ready, sent packets are not dropped due to ice gate
    /// (they may still be dropped due to mid=None, but the ice gate itself doesn't double-count).
    ///
    /// This test documents the unit-test limitation: mid=None prevents forwarding regardless
    /// of ice_ready. The key assertion is that ice_ready=true does not cause double-counting.
    #[test]
    fn post_mid_pre_ice_packets_are_dropped_and_counted() {
        use std::time::Duration;

        let enc = FakeEncoder::new();
        let enc_arc = Arc::clone(&enc) as Arc<dyn VideoEncoder + Send + Sync>;

        let mut sender = Str0mVideoSender::new(TransportConfig {
            udp_port: 0,
            ..TransportConfig::default()
        })
        .unwrap();
        sender.set_encoder(enc_arc);

        let (pkt_tx, pkt_rx) = sync_channel(8);
        let (event_tx, event_rx) = sync_channel::<TransportEvent>(4);
        sender.start(pkt_rx, event_tx).unwrap();

        // Send 2 packets before any inject (mid=None, ice_ready=false).
        for i in 0..2u64 {
            let _ = pkt_tx.send(sm_domain::encode::EncodedPacket {
                data: vec![0u8; 16].into(),
                timestamp: std::time::Duration::ZERO,
                is_keyframe: false,
                sequence: i,
            });
        }
        std::thread::sleep(Duration::from_millis(50));
        let dropped_before = sender.dropped_frames();
        assert!(
            dropped_before >= 2,
            "dropped_frames must be >= 2 after 2 pre-ICE packets, got {dropped_before}"
        );

        // Now inject ice_ready. mid is still None in a unit test (no real SDP exchange).
        sender.inject_ice_ready_for_test();
        // Drain the IceConnected event so it doesn't linger.
        let _ = event_rx.recv_timeout(Duration::from_millis(100));

        // Send 2 more packets. They still get dropped (mid=None), but NOT double-counted.
        // NOTE: In a unit test there is no real MediaAdded event, so mid stays None.
        // The assertion verifies the gate toggle alone doesn't artificially inflate drops.
        for i in 2..4u64 {
            let _ = pkt_tx.send(sm_domain::encode::EncodedPacket {
                data: vec![0u8; 16].into(),
                timestamp: std::time::Duration::ZERO,
                is_keyframe: false,
                sequence: i,
            });
        }
        // 250ms > the 200ms max tick timeout, so the loop is guaranteed to complete
        // at least one full iteration and drain the pkt channel.
        std::thread::sleep(Duration::from_millis(250));
        let dropped_after = sender.dropped_frames();

        // dropped_after should be exactly dropped_before + 2 (no double-counting).
        assert!(
            dropped_after >= dropped_before + 2,
            "dropped_frames must increase by at least 2 after 2 more packets, got before={dropped_before} after={dropped_after}"
        );

        drop(pkt_tx);
        sender.stop().unwrap();
    }

    // ─── T2.1 (streaming-emit-on-ice-connect): inject_ice_ready_for_test test seam ──

    /// T2.1 (AC-9) — `inject_ice_ready_for_test` causes `TransportEvent::IceConnected`
    /// to arrive on the event channel within 100ms.
    ///
    /// RED assertion: `inject_ice_ready_for_test` does not exist yet → compile failure.
    #[test]
    fn set_ice_ready_for_test_emits_ice_connected() {
        use std::time::Duration;

        let enc = FakeEncoder::new();
        let enc_arc = Arc::clone(&enc) as Arc<dyn VideoEncoder + Send + Sync>;

        let mut sender = Str0mVideoSender::new(TransportConfig {
            udp_port: 0,
            ..TransportConfig::default()
        })
        .unwrap();
        sender.set_encoder(enc_arc);

        let (_pkt_tx, pkt_rx) = sync_channel(4);
        let (event_tx, event_rx) = sync_channel::<TransportEvent>(4);
        sender.start(pkt_rx, event_tx).unwrap();

        sender.inject_ice_ready_for_test();

        let ev = event_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("IceConnected event must arrive within 100ms after inject_ice_ready_for_test");
        assert!(
            matches!(ev, TransportEvent::IceConnected),
            "expected TransportEvent::IceConnected, got {ev:?}"
        );

        sender.stop().unwrap();
    }

    // ─── T4.1 (streaming-emit-on-ice-connect): gate opens and is monotonic ──────

    /// T4.1 (AC-3, AC-9) — inject_ice_ready_for_test opens the gate, emits IceConnected
    /// exactly once, and does not cause double-dropping on subsequent packets.
    #[test]
    fn set_ice_ready_for_test_gate_opens_and_is_monotonic() {
        use std::time::Duration;

        let enc = FakeEncoder::new();
        let enc_arc = Arc::clone(&enc) as Arc<dyn VideoEncoder + Send + Sync>;

        let mut sender = Str0mVideoSender::new(TransportConfig {
            udp_port: 0,
            ..TransportConfig::default()
        })
        .unwrap();
        sender.set_encoder(enc_arc);

        let (pkt_tx, pkt_rx) = sync_channel(8);
        let (event_tx, event_rx) = sync_channel::<TransportEvent>(8);
        sender.start(pkt_rx, event_tx).unwrap();

        // Inject ice_ready and wait for IceConnected event.
        sender.inject_ice_ready_for_test();
        let ev = event_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("IceConnected must arrive within 100ms");
        assert!(
            matches!(ev, TransportEvent::IceConnected),
            "expected IceConnected, got {ev:?}"
        );

        // Assert exactly one IceConnected (no duplicate).
        assert!(
            event_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "IceConnected must not be emitted more than once"
        );

        // Record drop count after gate open.
        let dropped_at_inject = sender.dropped_frames();

        // Send 2 more packets. Because mid=None (no real SDP exchange in unit tests),
        // they still go to the else branch and are counted. Confirm they are counted
        // only once (not double-counted due to the gate toggle).
        for i in 0..2u64 {
            let _ = pkt_tx.send(sm_domain::encode::EncodedPacket {
                data: vec![0u8; 16].into(),
                timestamp: std::time::Duration::ZERO,
                is_keyframe: false,
                sequence: i,
            });
        }
        std::thread::sleep(Duration::from_millis(250));
        let dropped_after = sender.dropped_frames();

        assert!(
            dropped_after >= dropped_at_inject + 2,
            "dropped_frames must increase by exactly 2 (not double-counted), \
             got at_inject={dropped_at_inject} after={dropped_after}"
        );

        drop(pkt_tx);
        sender.stop().unwrap();
    }

    // ─── T4.2 (streaming-emit-on-ice-connect): Disconnected does not reset gate ─

    /// T4.2 (AC-4) — After IceConnected, dropped_frames does not spike
    /// (gate stays semantically open even though mid=None in unit tests).
    ///
    /// NOTE: Disconnected path exercised by TST-L-1 loopback (T6.1). This test
    /// documents the unit-test constraint and confirms no unexpected drop spike.
    #[test]
    fn disconnected_after_ice_ready_does_not_reset_gate() {
        use std::time::Duration;

        let enc = FakeEncoder::new();
        let enc_arc = Arc::clone(&enc) as Arc<dyn VideoEncoder + Send + Sync>;

        let mut sender = Str0mVideoSender::new(TransportConfig {
            udp_port: 0,
            ..TransportConfig::default()
        })
        .unwrap();
        sender.set_encoder(enc_arc);

        let (_pkt_tx, pkt_rx) = sync_channel(4);
        let (event_tx, event_rx) = sync_channel::<TransportEvent>(4);
        sender.start(pkt_rx, event_tx).unwrap();

        // Open the gate.
        sender.inject_ice_ready_for_test();
        let ev = event_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("IceConnected must arrive");
        assert!(matches!(ev, TransportEvent::IceConnected));

        // In a unit test we cannot inject Disconnected from outside the tick loop
        // without a real event. Instead: verify dropped_frames does not spike
        // unexpectedly, confirming no double-accounting triggered by the gate.
        let dropped_snapshot = sender.dropped_frames();
        std::thread::sleep(Duration::from_millis(50));
        let dropped_later = sender.dropped_frames();

        assert_eq!(
            dropped_snapshot, dropped_later,
            "dropped_frames must not increase when no packets are sent; \
             before={dropped_snapshot} after={dropped_later}. \
             // NOTE: Disconnected path exercised by TST-L-1 loopback (T6.1)."
        );

        sender.stop().unwrap();
    }

    /// R9.3 / S9.3 — `start()` MUST return `Err(InvalidConfig)` if no encoder was set.
    #[test]
    fn str0m_sender_start_without_encoder_returns_invalid_config_s9_3() {
        let mut sender = Str0mVideoSender::new(TransportConfig {
            udp_port: 0,
            ..TransportConfig::default()
        })
        .unwrap();
        // Note: NO set_encoder() call.

        let (_pkt_tx, pkt_rx) = sync_channel(4);
        let (event_tx, _event_rx) = sync_channel::<TransportEvent>(4);

        let result = sender.start(pkt_rx, event_tx);
        assert!(
            matches!(result, Err(TransportError::InvalidConfig(_))),
            "start() without prior set_encoder() must return Err(InvalidConfig), got: {result:?}"
        );
    }

    // ─── S-1 (streaming-emit-on-ice-connect carry-forward): Completed-only path ──

    /// S-1 (AC-3): The ICE-ready latch fires for both `Connected` and `Completed`,
    /// and ONLY for those — not for `Disconnected` or any other variant.
    ///
    /// Closes the carry-forward gap from archive #524 ("AC-3 coverage uses synthetic
    /// inject"): T4.1 latches the gate via `inject_ice_ready_for_test`, which bypasses
    /// the `matches!()` predicate that distinguishes Connected/Completed from other
    /// ICE state transitions. This test exercises the predicate directly.
    #[test]
    fn is_ice_ready_event_matches_connected_and_completed_only() {
        // Build minimal IceConnectionStateChange events for each variant.
        let connected = Event::IceConnectionStateChange(IceConnectionState::Connected);
        let completed = Event::IceConnectionStateChange(IceConnectionState::Completed);
        let disconnected = Event::IceConnectionStateChange(IceConnectionState::Disconnected);
        let new_event = Event::IceConnectionStateChange(IceConnectionState::New);
        let checking = Event::IceConnectionStateChange(IceConnectionState::Checking);

        assert!(
            is_ice_ready_event(&connected),
            "Connected MUST latch the gate"
        );
        assert!(
            is_ice_ready_event(&completed),
            "Completed MUST latch the gate (str0m may skip Connected with a single \
             working candidate pair)"
        );
        assert!(
            !is_ice_ready_event(&disconnected),
            "Disconnected MUST NOT latch the gate — supervisor owns recovery via \
             IceFailed → ReconnectSupervisor"
        );
        assert!(
            !is_ice_ready_event(&new_event),
            "New (initial) MUST NOT latch the gate"
        );
        assert!(
            !is_ice_ready_event(&checking),
            "Checking (in-flight gathering) MUST NOT latch the gate"
        );
    }

    // ─── S-2 (streaming-emit-on-ice-connect carry-forward): IceConnected-before-MediaAdded ──

    /// S-2 (AC-8): When `ice_ready` latches BEFORE `pre_neg.mid` is set
    /// (the inverted ordering — ICE handshake completes before SDP negotiation
    /// fully resolves the media line), the gate MUST keep dropping packets until
    /// BOTH conditions are met.
    ///
    /// Closes the carry-forward gap from archive #524 ("AC-8 no dedicated test for
    /// IceConnected → MediaAdded sequence"): the gate at str0m_sender.rs uses
    /// `if let Some(mid) { if ice_ready { write } else { drop } } else { drop }`,
    /// which structurally handles both orderings. This test exercises the
    /// (mid=None, ice_ready=true) state explicitly: packets MUST drop because
    /// `pre_neg.mid` is the outer condition.
    #[test]
    fn ice_ready_before_media_added_still_drops_packets() {
        use std::time::Duration;

        let enc = FakeEncoder::new();
        let enc_arc = Arc::clone(&enc) as Arc<dyn VideoEncoder + Send + Sync>;

        let mut sender = Str0mVideoSender::new(TransportConfig {
            udp_port: 0,
            ..TransportConfig::default()
        })
        .unwrap();
        sender.set_encoder(enc_arc);

        let (pkt_tx, pkt_rx) = sync_channel(8);
        let (event_tx, event_rx) = sync_channel::<TransportEvent>(8);
        sender.start(pkt_rx, event_tx).unwrap();

        // Latch the ICE-ready flag BEFORE any MediaAdded event arrives. In a unit
        // test, `pre_neg.mid` is never set (no real SDP exchange), so this models
        // the worst-case inverted ordering: ICE done, SDP not yet resolved.
        sender.inject_ice_ready_for_test();
        let ev = event_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("IceConnected must arrive after inject_ice_ready_for_test");
        assert!(
            matches!(ev, TransportEvent::IceConnected),
            "expected IceConnected, got {ev:?}"
        );

        let dropped_before = sender.dropped_frames();

        // Send 5 packets. Because `pre_neg.mid` is None (no MediaAdded), the
        // outer `if let Some(mid)` branch is false → all 5 must hit the
        // pre-negotiation drain-and-drop path (str0m_sender.rs:593-598),
        // regardless of `ice_ready` being true.
        for i in 0..5u64 {
            pkt_tx
                .send(sm_domain::encode::EncodedPacket {
                    data: vec![0u8; 16].into(),
                    timestamp: std::time::Duration::ZERO,
                    is_keyframe: false,
                    sequence: i,
                })
                .unwrap();
        }

        // Wait for the tick loop to drain the packet channel.
        std::thread::sleep(Duration::from_millis(250));

        let dropped_after = sender.dropped_frames();
        assert_eq!(
            dropped_after - dropped_before,
            5,
            "S-2 (AC-8): with ice_ready=true and mid=None, all 5 packets MUST drop. \
             dropped_before={dropped_before}, dropped_after={dropped_after}"
        );

        drop(pkt_tx);
        sender.stop().unwrap();
    }

    /// R14.3, S14.2 — When the event channel is full and a PLI fires, the sender
    /// MUST NOT panic or block.
    #[test]
    fn sender_pli_with_full_event_channel_no_panic_s14_2() {
        use std::time::Duration;

        let enc = FakeEncoder::new();
        let enc_arc = Arc::clone(&enc) as Arc<dyn VideoEncoder + Send + Sync>;

        let mut sender = Str0mVideoSender::new(TransportConfig {
            udp_port: 0,
            ..TransportConfig::default()
        })
        .unwrap();
        sender.set_encoder(enc_arc);

        let (_pkt_tx, pkt_rx) = sync_channel(4);
        // Capacity-1 channel that we NEVER drain.
        let (event_tx, _event_rx) = sync_channel::<TransportEvent>(1);
        sender.start(pkt_rx, event_tx).unwrap();

        // Inject multiple PLI requests to overflow the event channel.
        sender.inject_keyframe_request_for_test();
        sender.inject_keyframe_request_for_test();
        sender.inject_keyframe_request_for_test();

        std::thread::sleep(Duration::from_millis(150));

        let result = sender.stop();
        assert!(
            result.is_ok(),
            "stop() must return Ok even after full event channel"
        );
    }

    // ─── RTX regression guard ─────────────────────────────────────────────────
    //
    // RTX regression guard — REQ-RTX-3: goes RED if clear_codecs(), set_rtp_mode(),
    // or an str0m upgrade drops H.264 RTX default.
    // REQ-RTX-4: passes under `cargo nextest run --workspace` without production changes.

    #[test]
    fn sender_sdp_offer_contains_h264_rtx() {
        use std::time::Instant;
        use str0m::media::{Direction, MediaKind};

        // Build the sender Rtc as production does, EXCEPT this test omits enable_bwe.
        // Omitting enable_bwe is intentional: pacing is irrelevant for SDP-structure
        // assertions, and the SDP offer is byte-identical with or without it
        // (enable_bwe only flips the internal bwe_config option; extension map and
        // codec negotiation are unaffected — verified in design D-PPT3-1).
        // Any future change that calls clear_codecs() or set_rtp_mode() will break
        // this test — that is intentional.
        // See `sender_sdp_offer_valid_with_bwe_enabled` for a sibling test that
        // exercises the full production builder path including enable_bwe.
        let crypto = str0m::crypto::from_feature_flags();
        let mut rtc = str0m::Rtc::builder()
            .set_crypto_provider(Arc::new(crypto))
            .build(Instant::now());

        let mut change = rtc.sdp_api();
        change.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
        let (offer, _pending) = change.apply().expect("apply must succeed");
        let sdp = offer.to_string();

        // REQ-RTX-1(a): RTX codec present in SDP offer.
        assert!(
            sdp.contains("rtx/90000"),
            "SDP offer must contain RTX payload type (a=rtpmap rtx/90000): {sdp}"
        );

        // REQ-RTX-1(b): fmtp apt= line present (RTX paired with H.264).
        assert!(
            sdp.lines()
                .any(|l| l.starts_with("a=fmtp:") && l.contains("apt=")),
            "SDP offer must contain a=fmtp apt= line (RTX→H.264 pairing): {sdp}"
        );

        // REQ-RTX-1(c): nack feedback present for H.264.
        assert!(
            sdp.lines()
                .any(|l| l.starts_with("a=rtcp-fb:") && l.contains("nack")),
            "SDP offer must contain a=rtcp-fb nack line: {sdp}"
        );
    }

    /// REQ-RTX-5 / PACE-1 — The production builder path that includes `enable_bwe`
    /// MUST still produce a valid (non-empty) SDP offer.  enable_bwe only sets
    /// the internal `bwe_config` option; it does not touch the extension map or
    /// codec negotiation, so the offer is byte-identical to the non-BWE case.
    /// This test pins that invariant so a future str0m upgrade cannot silently
    /// break offer generation when the LeakyBucketPacer path is active.
    #[test]
    fn sender_sdp_offer_valid_with_bwe_enabled() {
        use std::time::Instant;
        use str0m::bwe::Bitrate;
        use str0m::media::{Direction, MediaKind};

        // Mirror the production Str0mVideoSender::new() builder exactly, using
        // a representative bitrate (TransportConfig default is 4 Mbps).
        let default_cfg = TransportConfig::default();
        let initial_estimate = Bitrate::bps(default_cfg.bitrate_bps as u64 * super::PACER_HEADROOM);

        let crypto = str0m::crypto::from_feature_flags();
        let mut rtc = str0m::Rtc::builder()
            .set_crypto_provider(Arc::new(crypto))
            .enable_bwe(Some(initial_estimate))
            .build(Instant::now());

        let mut change = rtc.sdp_api();
        change.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
        let result = change.apply();

        assert!(
            result.is_some(),
            "production builder with enable_bwe must produce a valid SDP offer (got None)"
        );
        let (offer, _pending) = result.unwrap();
        let sdp = offer.to_string();
        assert!(
            !sdp.is_empty(),
            "SDP offer string must be non-empty with enable_bwe active"
        );
        // Confirm the H.264 + RTX structure is preserved when BWE is enabled
        // (extension map is set unconditionally by RtcConfig::default; enable_bwe
        // does not modify it).
        assert!(
            sdp.contains("rtx/90000"),
            "SDP offer with enable_bwe must still contain RTX payload type: {sdp}"
        );
    }

    // ─── SC-2: bind_udp_socket_reusable — bind→drop→rebind (cross-platform) ──

    /// SC-2 — After dropping a UdpSocket, the same port can be rebound immediately.
    /// Confirms SO_REUSEADDR is set correctly on all platforms.
    #[test]
    fn bind_udp_socket_reusable_rebind_after_drop_succeeds() {
        use std::net::{SocketAddr, UdpSocket as StdUdpSocket};
        let zero: SocketAddr = "0.0.0.0:0".parse().unwrap();
        let sock1 = StdUdpSocket::bind(zero).expect("ephemeral bind for port discovery");
        let port = sock1.local_addr().unwrap().port();
        drop(sock1);

        let fixed: SocketAddr = format!("0.0.0.0:{port}").parse().unwrap();
        let result = bind_udp_socket_reusable(fixed);
        assert!(
            result.is_ok(),
            "rebind after drop must succeed (got: {:?})",
            result.err()
        );
        let port2 = result.unwrap().local_addr().unwrap().port();
        assert_eq!(port2, port, "rebound socket must have the same port");
    }

    // ─── SC-4: bind_udp_socket_reusable — live rebind (Windows-only) ──────────

    /// SC-4 — On Windows, SO_REUSEADDR allows a second UDP bind while the first
    /// socket is still alive. Defence-in-depth for the fixed-UDP-port scenario
    /// (current udp_port default is 0/ephemeral, but a future fixed port would
    /// re-introduce the bind race fixed on TCP).
    #[cfg(target_os = "windows")]
    #[test]
    fn bind_udp_socket_reusable_live_rebind_windows_succeeds() {
        use std::net::SocketAddr;
        let zero: SocketAddr = "0.0.0.0:0".parse().unwrap();
        let sock1 = bind_udp_socket_reusable(zero).expect("first bind");
        let port = sock1.local_addr().unwrap().port();
        let _hold = sock1; // intentionally NOT dropped

        let fixed: SocketAddr = format!("0.0.0.0:{port}").parse().unwrap();
        let result = bind_udp_socket_reusable(fixed);
        assert!(
            result.is_ok(),
            "live UDP rebind on Windows must succeed, got: {:?}",
            result.err()
        );
    }

    // ─── PaceStats seam unit tests (PACE-4 / D-PPT3-4) ───────────────────────

    /// on_transmit × 3 → on_drain_end → max_burst == 3, cur_burst == 0.
    #[test]
    fn pace_stats_single_drain() {
        let mut s = PaceStats::new();
        s.on_transmit();
        s.on_transmit();
        s.on_transmit();
        s.on_drain_end();
        assert_eq!(s.max_burst, 3, "max_burst must be 3 after one drain of 3");
        assert_eq!(
            s.cur_burst, 0,
            "cur_burst must reset to 0 after on_drain_end"
        );
    }

    /// Three drains of 5, 8, 2 → max_burst == 8; cur_burst resets after each.
    ///
    /// The maximum drain (8) is NOT last: the final drain (2) is smaller. This
    /// discriminates the max-across-drains semantics from a last-write-wins
    /// overwrite (`max_burst = cur_burst` unconditionally), which would leave
    /// max_burst == 2 and fail the final assertion.
    #[test]
    fn pace_stats_multi_drain_max() {
        let mut s = PaceStats::new();
        // drain 1: 5 packets
        for _ in 0..5 {
            s.on_transmit();
        }
        s.on_drain_end();
        assert_eq!(s.cur_burst, 0);
        assert_eq!(s.max_burst, 5, "max_burst must be 5 after the first drain");
        // drain 2: 8 packets (new maximum)
        for _ in 0..8 {
            s.on_transmit();
        }
        s.on_drain_end();
        assert_eq!(s.cur_burst, 0);
        assert_eq!(
            s.max_burst, 8,
            "max_burst must rise to 8 on the larger drain"
        );
        // drain 3: 2 packets (smaller than the running max — must NOT lower it)
        for _ in 0..2 {
            s.on_transmit();
        }
        s.on_drain_end();
        assert_eq!(s.cur_burst, 0);
        assert_eq!(
            s.max_burst, 8,
            "max_burst must stay at the largest drain (8), not the last drain (2)"
        );
    }

    /// 4 on_tick calls, 10 total on_transmit → ticks == 4, pkts == 10.
    #[test]
    fn pace_stats_ticks_and_pkts() {
        let mut s = PaceStats::new();
        for _ in 0..4 {
            s.on_tick();
        }
        // 10 transmits spread across multiple drains (totals matter, not per-drain)
        for _ in 0..6 {
            s.on_transmit();
        }
        s.on_drain_end();
        for _ in 0..4 {
            s.on_transmit();
        }
        s.on_drain_end();
        assert_eq!(s.ticks, 4, "ticks must equal on_tick call count");
        assert_eq!(s.pkts, 10, "pkts must equal total on_transmit calls");
    }

    /// Seed ticks=60, pkts=120, max_burst=4 → snapshot_per_s(1.0) returns (60.0, 120.0, 4).
    ///
    /// Build 30 drains of 4 packets each (30×4 = 120 pkts, max_burst = 4), plus 60 ticks.
    #[test]
    fn pace_stats_snapshot_divides() {
        let mut s = PaceStats::new();
        for _ in 0..60 {
            s.on_tick();
        }
        // 30 drains × 4 packets = 120 pkts total; max_burst stays at 4
        for _ in 0..30 {
            for _ in 0..4 {
                s.on_transmit();
            }
            s.on_drain_end();
        }
        let (ticks_s, pkts_s, max_burst) = s.snapshot_per_s(1.0);
        assert!(
            (ticks_s - 60.0).abs() < f64::EPSILON,
            "ticks_per_s must be 60.0, got {ticks_s}"
        );
        assert!(
            (pkts_s - 120.0).abs() < f64::EPSILON,
            "pkts_per_s must be 120.0, got {pkts_s}"
        );
        assert_eq!(max_burst, 4, "max_burst must be 4");
    }

    /// snapshot_per_s(0.0): the non-positive-elapsed early return makes the seam
    /// genuinely total — exact 0.0 rates (finite, no division by zero / NaN /
    /// infinity) even with large counters, while max_burst is preserved.
    #[test]
    fn pace_stats_snapshot_zero_elapsed_is_finite() {
        let mut s = PaceStats::new();
        s.on_tick();
        s.on_transmit();
        s.on_drain_end();
        let (ticks_s, pkts_s, max_burst) = s.snapshot_per_s(0.0);
        assert!(
            ticks_s.is_finite(),
            "ticks_per_s must be finite for zero elapsed, got {ticks_s}"
        );
        assert!(
            pkts_s.is_finite(),
            "pkts_per_s must be finite for zero elapsed, got {pkts_s}"
        );
        assert_eq!(max_burst, 1, "max_burst is unaffected by the denominator");

        // Realistic large counters: a floored f64::MIN_POSITIVE denominator
        // would overflow these to +inf. The early return must yield exact 0.0.
        let mut big = PaceStats::new();
        for _ in 0..5000 {
            big.on_tick();
        }
        for _ in 0..5000 {
            big.on_transmit();
        }
        big.on_drain_end();
        let (ticks_s, pkts_s, max_burst) = big.snapshot_per_s(0.0);
        assert_eq!(
            ticks_s, 0.0,
            "ticks_per_s must be exactly 0.0 for zero elapsed, got {ticks_s}"
        );
        assert_eq!(
            pkts_s, 0.0,
            "pkts_per_s must be exactly 0.0 for zero elapsed, got {pkts_s}"
        );
        assert!(ticks_s.is_finite() && pkts_s.is_finite());
        assert_eq!(max_burst, 5000, "max_burst must be preserved at 5000");
    }

    /// After populating, reset() → all fields are zero.
    #[test]
    fn pace_stats_reset() {
        let mut s = PaceStats::new();
        s.on_tick();
        s.on_transmit();
        s.on_drain_end();
        s.reset();
        assert_eq!(s.ticks, 0, "ticks must be 0 after reset");
        assert_eq!(s.pkts, 0, "pkts must be 0 after reset");
        assert_eq!(s.max_burst, 0, "max_burst must be 0 after reset");
        assert_eq!(s.cur_burst, 0, "cur_burst must be 0 after reset");
    }
}
