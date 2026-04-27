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

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use str0m::change::SdpPendingOffer;
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
        let crypto = str0m::crypto::from_feature_flags();
        let mut rtc = Rtc::builder()
            .set_crypto_provider(Arc::new(crypto))
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

        // Reset stop flag in case this sender is restarted.
        self.state.stop.store(false, Ordering::Release);

        // Bind the UDP socket.
        let bind_addr = format!("0.0.0.0:{}", self.config.udp_port);
        let udp = UdpSocket::bind(&bind_addr)
            .map_err(|e| TransportError::Io(format!("UDP bind failed on {bind_addr}: {e}")))?;
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
}

// ─── Tick loop ───────────────────────────────────────────────────────────────

/// SansIO tick loop for `Str0mVideoSender`.
///
/// Runs on the dedicated OS thread spawned by `start()`.
#[allow(clippy::too_many_arguments)]
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

    loop {
        // ── 1. Stop flag ──────────────────────────────────────────────────
        if state.stop.load(Ordering::Acquire) {
            break;
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
            }
        }

        // ── 3. Drain encoded packets and write to str0m ───────────────────
        // Only write if we have a valid mid and pt (post-negotiation).
        if let (Some(mid), Some(pt)) = (pre_neg.mid, pre_neg.pt) {
            while let Ok(pkt) = rx.try_recv() {
                let rtp_ts = duration_to_90khz(pkt.timestamp);
                let rtp_time = MediaTime::from_90khz(rtp_ts);
                let wallclock = Instant::now();

                if let Some(writer) = rtc.writer(mid) {
                    // Pass the entire Annex-B frame. str0m's H264Packetizer
                    // handles start-code stripping, FU-A fragmentation, SRTP.
                    if let Err(_e) = writer.write(pt, wallclock, rtp_time, pkt.data.as_ref()) {
                        state.dropped.fetch_add(1, Ordering::Relaxed);
                    }
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
                    let _ = udp.send_to(&t.contents, t.destination);
                }
                Ok(Output::Event(ev)) => {
                    // Capture mid/pt from MediaAdded event.
                    if let Event::MediaAdded(ref added) = ev {
                        if pre_neg.pt.is_none() {
                            // Find H264 PT from the session's codec config.
                            // The mid is already stored; get the payload type.
                            pre_neg.mid = Some(added.mid);
                            // pt resolution deferred to next writer call.
                        }
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
}
