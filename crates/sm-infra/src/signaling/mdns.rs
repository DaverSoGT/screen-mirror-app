//! mDNS auto-discovery + TCP control channel signaling adapter.
//!
//! [`MdnsSignaling`] implements [`sm_domain::signaling::Signaling`]. It publishes
//! (sender role) or discovers (receiver role) the `_screen-mirror._tcp.local.` service
//! via mDNS and then exchanges SDP/ICE frames over a direct TCP connection using the
//! length-prefixed JSON protocol defined in [`crate::signaling::wire`].
//!
//! # Thread model
//!
//! `start()` spawns exactly one OS thread (`"sm-signaling-mdns"`). That thread drives
//! the mDNS daemon (via `mdns-sd`) and the TCP control socket. Once the TCP channel
//! is open, it loops reading frames and emitting [`SignalingEvent`]s. Outbound frames
//! are queued via an inbox and written on the same thread. `stop()` sets an
//! [`AtomicBool`] stop flag and joins the thread.
//!
//! # Usage
//!
//! ```rust,no_run
//! use sm_infra::signaling::mdns::MdnsSignaling;
//! use sm_domain::signaling::{Signaling, SignalingConfig, SignalingRole, SignalingEvent};
//! use std::sync::mpsc::sync_channel;
//!
//! let config = SignalingConfig {
//!     role: SignalingRole::Sender,
//!     ..Default::default()
//! };
//! let mut sig = MdnsSignaling::new(config).unwrap();
//! let (event_tx, event_rx) = sync_channel::<SignalingEvent>(8);
//! sig.start(event_tx).unwrap();
//! // ... exchange offer/answer/candidates ...
//! sig.stop().unwrap();
//! ```

use std::io::{self, BufReader, BufWriter, Read};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

use sm_domain::signaling::{
    IceCandidate, SdpAnswer, SdpOffer, Signaling, SignalingConfig, SignalingError, SignalingEvent,
    SignalingRole,
};
use sm_domain::supervisor::SupervisorSignal;

use crate::signaling::wire::{MAX_FRAME_BYTES, SignalingFrame, write_frame};

/// Write timeout for `publish_reconnect_request` / `publish_reconnect_ack`.
///
/// Per design §3 (TCP reuse heuristic): if the write does not complete within 2s,
/// the caller should fall back to the mDNS reset path.
const RECONNECT_WRITE_TIMEOUT: Duration = Duration::from_secs(2);

// ─── Constants ───────────────────────────────────────────────────────────────

/// mDNS service type for screen-mirror.
const SERVICE_TYPE: &str = "_screen-mirror._tcp.local.";

/// Instance name used when registering the sender service.
const INSTANCE_NAME: &str = "screen-mirror";

/// mDNS/TCP discovery timeout before `PeerNotFound` is reported.
///
/// D-7: extended from 10s to 30s to cover sender republish latency after S-1 supervisor
/// wakes on Bye. The sender republishes within ≤2s of receiving PeerBye via the supervisor;
/// 30s provides a 28s buffer for Windows mDNS jitter, anti-virus scan delays, and
/// USB-Ethernet hotplug events — all while satisfying the 12s reconnect budget.
const DISCOVER_TIMEOUT: Duration = Duration::from_secs(30);

/// Read timeout for the TCP frame loop — allows periodic stop-flag checks.
const READ_TIMEOUT: Duration = Duration::from_millis(200);

// ─── Internal control messages ────────────────────────────────────────────────

/// Outbound frames queued from the public API into the signaling thread.
#[derive(Debug)]
enum MdnsControl {
    /// Offer to be forwarded to the connected peer.
    Offer(SdpOffer),
    /// Answer to be forwarded to the connected peer.
    Answer(SdpAnswer),
    /// ICE candidate to be forwarded to the connected peer.
    Candidate(IceCandidate),
    /// Reconnect request to be forwarded to the connected peer.
    ///
    /// Published when local `IceFailed` or `ConnectionLost` is detected.
    ReconnectRequest {
        attempt: u8,
        requester_role: SignalingRole,
        session_nonce: u64,
    },
    /// Reconnect acknowledgment to be forwarded to the connected peer.
    ///
    /// Published by the losing side in a simultaneous-detect race, or the
    /// responding side in a one-sided detect.
    ReconnectAck { attempt: u8, session_nonce: u64 },
}

// ─── MdnsSignaling ────────────────────────────────────────────────────────────

/// mDNS auto-discovery + TCP control channel signaling adapter.
///
/// Implements [`Signaling`]. Role is fixed at construction:
/// - **Sender**: publishes `_screen-mirror._tcp.local.`, listens for TCP connections.
/// - **Receiver**: browses for `_screen-mirror._tcp.local.`, connects TCP to sender.
///
/// Once the TCP connection is established, both sides exchange [`SignalingFrame`]s
/// (length-prefixed JSON). The thread emits [`SignalingEvent`]s on the channel
/// injected via `start(event_tx)`.
///
/// # Network notes
///
/// Requires a working multicast interface. Tests that exercise mDNS discovery are
/// annotated `#[ignore]` per R7.5. Unit tests for framing and error mapping do NOT
/// require network access.
pub struct MdnsSignaling {
    /// Runtime configuration.
    config: SignalingConfig,
    /// Shared stop flag — raised by `stop()` and `Drop`.
    stop: Arc<AtomicBool>,
    /// Outbound control inbox (public API → signaling thread).
    inbox: Arc<Mutex<Vec<MdnsControl>>>,
    /// Thread handle (None before `start()` and after `stop()`).
    handle: Option<JoinHandle<()>>,
    /// Supervisor signal channel — used by the frame loop to route incoming
    /// `ReconnectRequest` and `ReconnectAck` frames to the reconnect supervisor.
    ///
    /// `None` until `set_supervisor_signal_tx` is called. When `None`, reconnect
    /// frames are silently consumed (backward-compatible — frame_to_event returns None).
    supervisor_signal_tx: Arc<Mutex<Option<SyncSender<SupervisorSignal>>>>,
}

impl Signaling for MdnsSignaling {
    /// Construct an `MdnsSignaling` instance. No threads started, no network touched.
    fn new(config: SignalingConfig) -> Result<Self, SignalingError> {
        Ok(Self {
            config,
            stop: Arc::new(AtomicBool::new(false)),
            inbox: Arc::new(Mutex::new(Vec::new())),
            handle: None,
            supervisor_signal_tx: Arc::new(Mutex::new(None)),
        })
    }

    /// Begin signaling. Spawns the `"sm-signaling-mdns"` OS thread.
    ///
    /// Returns `Err(AlreadyRunning)` if called twice without an intervening `stop()`.
    fn start(&mut self, event_tx: SyncSender<SignalingEvent>) -> Result<(), SignalingError> {
        if self.handle.is_some() {
            return Err(SignalingError::AlreadyRunning);
        }
        self.stop.store(false, Ordering::Release);

        let config = self.config.clone();
        let stop = Arc::clone(&self.stop);
        let inbox = Arc::clone(&self.inbox);
        let supervisor_signal_tx = Arc::clone(&self.supervisor_signal_tx);

        let handle = thread::Builder::new()
            .name("sm-signaling-mdns".to_string())
            .spawn(move || {
                run_signaling_thread(config, stop, inbox, event_tx, supervisor_signal_tx);
            })
            .map_err(|e| SignalingError::Io(e.to_string()))?;

        self.handle = Some(handle);
        Ok(())
    }

    /// Queue an SDP offer to be written on the TCP channel.
    ///
    /// Returns `Err(NotRunning)` if `start()` has not been called or `stop()` was called.
    fn publish_local_offer(&self, offer: SdpOffer) -> Result<(), SignalingError> {
        if self.handle.is_none() {
            return Err(SignalingError::NotRunning);
        }
        self.inbox.lock().unwrap().push(MdnsControl::Offer(offer));
        Ok(())
    }

    /// Queue an SDP answer to be written on the TCP channel.
    ///
    /// Returns `Err(NotRunning)` if `start()` has not been called.
    fn publish_local_answer(&self, answer: SdpAnswer) -> Result<(), SignalingError> {
        if self.handle.is_none() {
            return Err(SignalingError::NotRunning);
        }
        self.inbox.lock().unwrap().push(MdnsControl::Answer(answer));
        Ok(())
    }

    /// Queue an ICE candidate to be written on the TCP channel.
    ///
    /// Returns `Err(NotRunning)` if `start()` has not been called.
    fn publish_local_candidate(&self, cand: IceCandidate) -> Result<(), SignalingError> {
        if self.handle.is_none() {
            return Err(SignalingError::NotRunning);
        }
        self.inbox
            .lock()
            .unwrap()
            .push(MdnsControl::Candidate(cand));
        Ok(())
    }

    /// Stop signaling. Idempotent. Joins the thread.
    fn stop(&mut self) -> Result<(), SignalingError> {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            handle.join().ok();
        }
        Ok(())
    }
}

impl MdnsSignaling {
    /// Register a supervisor signal channel so incoming `ReconnectRequest` and
    /// `ReconnectAck` frames are forwarded to the reconnect supervisor instead of
    /// being silently consumed.
    ///
    /// Call this BEFORE `start()` to ensure no frames are missed. Calling after
    /// `start()` is safe but may miss frames that arrive before the channel is set.
    pub fn set_supervisor_signal_tx(&self, tx: SyncSender<SupervisorSignal>) {
        *self.supervisor_signal_tx.lock().unwrap() = Some(tx);
    }

    /// Queue a `ReconnectRequest` frame to be written on the TCP channel.
    ///
    /// Uses the existing inbox mechanism — the frame loop writes it on the next
    /// inbox drain. Returns `Err(NotRunning)` if `start()` has not been called.
    ///
    /// Per design §3 TCP reuse heuristic: the TCP stream has a write timeout set to
    /// [`RECONNECT_WRITE_TIMEOUT`] (2s). If the write does not complete, the caller
    /// should invoke `reset()` for full mDNS rediscovery.
    pub fn publish_reconnect_request(
        &self,
        attempt: u8,
        requester_role: SignalingRole,
        session_nonce: u64,
    ) -> Result<(), SignalingError> {
        if self.handle.is_none() {
            return Err(SignalingError::NotRunning);
        }
        self.inbox
            .lock()
            .unwrap()
            .push(MdnsControl::ReconnectRequest {
                attempt,
                requester_role,
                session_nonce,
            });
        Ok(())
    }

    /// Queue a `ReconnectAck` frame to be written on the TCP channel.
    ///
    /// Returns `Err(NotRunning)` if `start()` has not been called.
    pub fn publish_reconnect_ack(
        &self,
        attempt: u8,
        session_nonce: u64,
    ) -> Result<(), SignalingError> {
        if self.handle.is_none() {
            return Err(SignalingError::NotRunning);
        }
        self.inbox.lock().unwrap().push(MdnsControl::ReconnectAck {
            attempt,
            session_nonce,
        });
        Ok(())
    }

    /// Perform a full mDNS reset: stop the current signaling instance and rebuild
    /// a new one with the same configuration.
    ///
    /// This is the TCP failure fallback path (design §3): when `publish_reconnect_request`
    /// fails or no `ReconnectAck` arrives within 2s, the supervisor calls `reset()` to
    /// tear down the stale TCP connection and rediscover the peer via mDNS.
    ///
    /// After `reset()`, callers MUST call `start(event_tx)` again with a fresh event channel.
    ///
    /// # Lifecycle
    ///
    /// The returned `MdnsSignaling` is in the same pre-start state as `new()`. The caller
    /// is responsible for re-registering the supervisor signal channel via
    /// `set_supervisor_signal_tx()` before calling `start()`.
    pub fn reset(self) -> Result<MdnsSignaling, SignalingError> {
        // `self` is consumed (moved), which calls Drop and stops the thread.
        // Construct a fresh instance with the same config.
        let config = self.config.clone();
        // Drop `self` — this calls `Stop::drop` which calls `stop()`.
        drop(self);
        MdnsSignaling::new(config)
    }
}

impl Drop for MdnsSignaling {
    /// Ensures the signaling thread is stopped when `MdnsSignaling` is dropped.
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

// ─── Frame → event mapping ────────────────────────────────────────────────────

/// Convert an inbound [`SignalingFrame`] into the matching [`SignalingEvent`].
///
/// Returns `None` for `Hello` — consumed silently as a protocol-version handshake.
/// Returns `None` for `ReconnectRequest`/`ReconnectAck` — these frames are forwarded
/// to the reconnect supervisor via `supervisor_signal_tx` (if set) instead of producing
/// a `SignalingEvent`. When `supervisor_signal_tx` is `None`, reconnect frames are
/// silently consumed (Phase 3 backward-compatible behavior).
/// All other variants map 1-to-1 to `SignalingEvent`.
pub(crate) fn frame_to_event(
    frame: SignalingFrame,
    supervisor_signal_tx: &Arc<Mutex<Option<SyncSender<SupervisorSignal>>>>,
) -> Option<SignalingEvent> {
    match frame {
        SignalingFrame::Hello { proto: _ } => None,
        SignalingFrame::Offer { sdp } => Some(SignalingEvent::OfferReceived(SdpOffer(sdp))),
        SignalingFrame::Answer { sdp } => Some(SignalingEvent::AnswerReceived(SdpAnswer(sdp))),
        SignalingFrame::Candidate { sdp } => {
            Some(SignalingEvent::CandidateReceived(IceCandidate(sdp)))
        }
        SignalingFrame::Bye => {
            // D-5 (REQ-S1): ALSO send LocalFailure{PeerBye} to the supervisor when wired.
            // Best-effort try_send — if supervisor channel is None or full, the Closed
            // event still flows normally via the signaling drain (defense-in-depth).
            // This enables the S-1 eager wake: sender supervisor transitions to
            // AwaitingAck at t≈0 on Bye, before the transport detects IceFailed.
            if let Some(tx) = supervisor_signal_tx.lock().unwrap().as_ref() {
                let _ = tx.try_send(SupervisorSignal::LocalFailure {
                    trigger: sm_domain::session::ReconnectTrigger::PeerBye,
                });
            }
            Some(SignalingEvent::Closed)
        }
        SignalingFrame::ReconnectRequest {
            attempt,
            requester_role,
            session_nonce,
        } => {
            // Route to supervisor channel; do NOT produce a SignalingEvent.
            // `session_nonce` from the peer acts as the peer's nonce for the
            // role-equal tie-break fallback. `requester_role` is forwarded as
            // `peer_role` so the supervisor's role-aware tie-break (design #963 D1)
            // can elect the offerer (Sender) as the active reconnector — previously
            // this field was discarded (#962), making the tie-break role-blind.
            if let Some(tx) = supervisor_signal_tx.lock().unwrap().as_ref() {
                let _ = tx.try_send(SupervisorSignal::PeerRequest {
                    peer_nonce: session_nonce,
                    peer_role: requester_role,
                    attempt,
                });
            }
            None
        }
        SignalingFrame::ReconnectAck {
            attempt,
            session_nonce,
        } => {
            // Route to supervisor channel; do NOT produce a SignalingEvent.
            if let Some(tx) = supervisor_signal_tx.lock().unwrap().as_ref() {
                let _ = tx.try_send(SupervisorSignal::PeerAck {
                    session_nonce,
                    attempt,
                });
            }
            None
        }
    }
}

// ─── Thread entry point ───────────────────────────────────────────────────────

/// Dispatch to the sender or receiver thread based on role.
fn run_signaling_thread(
    config: SignalingConfig,
    stop: Arc<AtomicBool>,
    inbox: Arc<Mutex<Vec<MdnsControl>>>,
    event_tx: SyncSender<SignalingEvent>,
    supervisor_signal_tx: Arc<Mutex<Option<SyncSender<SupervisorSignal>>>>,
) {
    match config.role {
        SignalingRole::Sender => {
            run_sender_thread(config, stop, inbox, event_tx, supervisor_signal_tx)
        }
        SignalingRole::Receiver => {
            run_receiver_thread(config, stop, inbox, event_tx, supervisor_signal_tx)
        }
    }
}

// ─── Socket helpers ───────────────────────────────────────────────────────────

fn bind_tcp_listener_reusable(addr: SocketAddr) -> io::Result<TcpListener> {
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(addr),
        socket2::Type::STREAM,
        None,
    )?;
    // SO_REUSEADDR on Windows allows binding while a previous socket on the
    // same address is still LIVE (LISTEN state) — exactly what we need for
    // the supervisor rebuild race where the old MdnsSignaling thread still
    // holds 0.0.0.0:7889 via Arc clones in coordinator_hooks (engram #1417).
    // On Unix this only covers TIME_WAIT rebinds — also useful, never harmful.
    socket.set_reuse_address(true)?;
    socket.bind(&addr.into())?;
    socket.listen(128)?;
    Ok(socket.into())
}

// ─── Sender thread ────────────────────────────────────────────────────────────

fn run_sender_thread(
    config: SignalingConfig,
    stop: Arc<AtomicBool>,
    inbox: Arc<Mutex<Vec<MdnsControl>>>,
    event_tx: SyncSender<SignalingEvent>,
    supervisor_signal_tx: Arc<Mutex<Option<SyncSender<SupervisorSignal>>>>,
) {
    let port = config.control_port;

    // Bind TCP listener BEFORE mDNS registration so the receiver can connect
    // immediately after discovery.
    let bind_addr: SocketAddr = format!("0.0.0.0:{port}")
        .parse()
        .expect("static 0.0.0.0:{port} format is always a valid SocketAddr");
    let listener = match bind_tcp_listener_reusable(bind_addr) {
        Ok(l) => l,
        Err(e) => {
            emit_error(&event_tx, SignalingError::Io(e.to_string()));
            return;
        }
    };
    // Non-blocking accept so we can poll the stop flag while waiting.
    if let Err(e) = listener.set_nonblocking(true) {
        emit_error(&event_tx, SignalingError::Io(e.to_string()));
        return;
    }

    // Enumerate IPv4 addresses for mDNS registration.
    let ip_list = collect_ipv4_addrs();
    if ip_list.is_empty() {
        emit_error(
            &event_tx,
            SignalingError::Io("no IPv4 network interfaces found".to_string()),
        );
        return;
    }

    // Register mDNS service.
    let mdns = match ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            emit_error(&event_tx, SignalingError::Io(e.to_string()));
            return;
        }
    };

    let host_name = format!("{}.local.", ip_list[0]);
    let ip_str = ip_list[0].to_string();
    let props = [("role", "sender"), ("proto", "v1")];
    let service_info = match ServiceInfo::new(
        SERVICE_TYPE,
        INSTANCE_NAME,
        &host_name,
        ip_str.as_str(),
        port,
        &props[..],
    ) {
        Ok(s) => s,
        Err(e) => {
            emit_error(&event_tx, SignalingError::Io(e.to_string()));
            return;
        }
    };

    if let Err(e) = mdns.register(service_info) {
        emit_error(&event_tx, SignalingError::Io(e.to_string()));
        return;
    }

    // Accept one TCP connection (non-blocking with stop-flag polling).
    let stream = loop {
        if stop.load(Ordering::Acquire) {
            let _ = mdns.shutdown();
            return;
        }
        match listener.accept() {
            Ok((stream, addr)) => {
                let _ = emit(
                    &event_tx,
                    SignalingEvent::PeerFound {
                        host: addr.ip().to_string(),
                        port: addr.port(),
                    },
                );
                break stream;
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
                continue;
            }
            Err(e) => {
                emit_error(&event_tx, SignalingError::Io(e.to_string()));
                let _ = mdns.shutdown();
                return;
            }
        }
    };

    // D-8: mdns.shutdown() is called AFTER run_frame_loop returns so the mDNS service
    // stays published throughout the entire TCP session. A reconnecting receiver can
    // still discover the sender while streaming. When run_frame_loop exits (Bye, error,
    // or stop_flag), shutdown() sends the goodbye packet to clean up the mDNS entry.
    run_frame_loop(stream, stop, inbox, event_tx, supervisor_signal_tx);
    let _ = mdns.shutdown();
}

// ─── Receiver thread ──────────────────────────────────────────────────────────

fn run_receiver_thread(
    _config: SignalingConfig,
    stop: Arc<AtomicBool>,
    inbox: Arc<Mutex<Vec<MdnsControl>>>,
    event_tx: SyncSender<SignalingEvent>,
    supervisor_signal_tx: Arc<Mutex<Option<SyncSender<SupervisorSignal>>>>,
) {
    let mdns = match ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            emit_error(&event_tx, SignalingError::Io(e.to_string()));
            return;
        }
    };

    let browse_rx = match mdns.browse(SERVICE_TYPE) {
        Ok(r) => r,
        Err(e) => {
            emit_error(&event_tx, SignalingError::Io(e.to_string()));
            return;
        }
    };

    // Wait for a resolved service within the discovery timeout.
    let deadline = std::time::Instant::now() + DISCOVER_TIMEOUT;
    let resolved = loop {
        if stop.load(Ordering::Acquire) {
            let _ = mdns.shutdown();
            return;
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            emit_error(&event_tx, SignalingError::PeerNotFound);
            let _ = mdns.shutdown();
            return;
        }
        match browse_rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
            Ok(ServiceEvent::ServiceResolved(info)) => break info,
            Ok(_) => continue,
            Err(_) => continue,
        }
    };

    // Pick first IPv4 address from the resolved service.
    let peer_addr = resolved
        .get_addresses()
        .iter()
        .filter_map(|a| match a.to_ip_addr() {
            IpAddr::V4(v4) => Some(v4),
            _ => None,
        })
        .next();

    let peer_ip: Ipv4Addr = match peer_addr {
        Some(ip) => ip,
        None => {
            emit_error(
                &event_tx,
                SignalingError::Io("resolved service has no IPv4 address".to_string()),
            );
            let _ = mdns.shutdown();
            return;
        }
    };

    let peer_port = resolved.get_port();
    let _ = emit(
        &event_tx,
        SignalingEvent::PeerFound {
            host: peer_ip.to_string(),
            port: peer_port,
        },
    );
    let stream = match TcpStream::connect((peer_ip, peer_port)) {
        Ok(s) => s,
        Err(e) => {
            emit_error(&event_tx, SignalingError::Io(e.to_string()));
            let _ = mdns.shutdown();
            return;
        }
    };

    // D-8: mdns.shutdown() is called AFTER run_frame_loop returns so the mDNS service
    // stays published throughout the entire TCP session. The mDNS entry remains active
    // until the TCP session ends, ensuring a reconnecting sender can still be discovered
    // by other receivers. Shutdown sends the goodbye packet to clean up the mDNS entry.
    run_frame_loop(stream, stop, inbox, event_tx, supervisor_signal_tx);
    let _ = mdns.shutdown();
}

// ─── Shared TCP frame loop ────────────────────────────────────────────────────

/// Drive the TCP control channel: write outbound frames from inbox, read inbound
/// frames and emit `SignalingEvent`s. Runs until the stop flag is set or the
/// connection closes.
fn run_frame_loop(
    stream: TcpStream,
    stop: Arc<AtomicBool>,
    inbox: Arc<Mutex<Vec<MdnsControl>>>,
    event_tx: SyncSender<SignalingEvent>,
    supervisor_signal_tx: Arc<Mutex<Option<SyncSender<SupervisorSignal>>>>,
) {
    // Diagnostic: log the actual TCP endpoints so loopback/dup-connect can be
    // distinguished from cross-host. Should always be peer != local; equal
    // would indicate the writer is feeding the reader on the same machine.
    let peer = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|e| format!("<peer_addr err: {e}>"));
    let local = stream
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|e| format!("<local_addr err: {e}>"));
    eprintln!("[sm-signaling-frame-loop] connection up: local={local} peer={peer}");

    // Set read timeout so the loop can check the stop flag and drain the inbox.
    if let Err(e) = stream.set_read_timeout(Some(READ_TIMEOUT)) {
        emit_error(&event_tx, SignalingError::Io(e.to_string()));
        return;
    }

    let write_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            emit_error(&event_tx, SignalingError::Io(e.to_string()));
            return;
        }
    };

    // Set write timeout on the cloned stream so reconnect frame writes do not
    // block indefinitely on a half-open TCP connection (design §3, AC-6).
    // Note: on Windows (Winsock), write timeout via SO_SNDTIMEO may not always
    // fire reliably on half-open sockets; the 2s ack-timeout in the supervisor
    // is the primary guard against hanging.
    if let Err(e) = write_stream.set_write_timeout(Some(RECONNECT_WRITE_TIMEOUT)) {
        eprintln!("[sm-signaling-frame-loop] WARN: set_write_timeout failed: {e}");
        // Non-fatal: continue without write timeout.
    }

    let mut writer = BufWriter::new(write_stream);
    let mut reader = BufReader::new(stream);

    // Send Hello frame first.
    if let Err(e) = write_frame(
        &mut writer,
        &SignalingFrame::Hello {
            proto: "v1".to_string(),
        },
    ) {
        eprintln!("[sm-signaling-frame-loop] EXIT: hello write failed: {e}");
        emit_error(&event_tx, SignalingError::Io(e.to_string()));
        return;
    }

    loop {
        if stop.load(Ordering::Acquire) {
            eprintln!("[sm-signaling-frame-loop] EXIT: stop flag set, sending Bye");
            let _ = write_frame(&mut writer, &SignalingFrame::Bye);
            break;
        }

        // Drain outbound inbox → write frames.
        let pending: Vec<MdnsControl> = inbox.lock().unwrap().drain(..).collect();
        for msg in pending {
            let frame = match msg {
                MdnsControl::Offer(o) => SignalingFrame::Offer { sdp: o.0 },
                MdnsControl::Answer(a) => SignalingFrame::Answer { sdp: a.0 },
                MdnsControl::Candidate(c) => SignalingFrame::Candidate { sdp: c.0 },
                MdnsControl::ReconnectRequest {
                    attempt,
                    requester_role,
                    session_nonce,
                } => SignalingFrame::ReconnectRequest {
                    attempt,
                    requester_role,
                    session_nonce,
                },
                MdnsControl::ReconnectAck {
                    attempt,
                    session_nonce,
                } => SignalingFrame::ReconnectAck {
                    attempt,
                    session_nonce,
                },
            };
            let kind = match &frame {
                SignalingFrame::Offer { sdp } => format!("Offer (sdp={} bytes)", sdp.len()),
                SignalingFrame::Answer { sdp } => format!("Answer (sdp={} bytes)", sdp.len()),
                SignalingFrame::Candidate { sdp } => format!("Candidate (sdp={} bytes)", sdp.len()),
                SignalingFrame::Hello { proto } => format!("Hello (proto={proto})"),
                SignalingFrame::Bye => "Bye".to_string(),
                SignalingFrame::ReconnectRequest {
                    attempt,
                    session_nonce,
                    ..
                } => format!("ReconnectRequest (attempt={attempt}, nonce={session_nonce})"),
                SignalingFrame::ReconnectAck {
                    attempt,
                    session_nonce,
                } => format!("ReconnectAck (attempt={attempt}, nonce={session_nonce})"),
            };
            eprintln!("[sm-signaling-frame-loop] OUT → {kind}");
            if let Err(e) = write_frame(&mut writer, &frame) {
                eprintln!("[sm-signaling-frame-loop] write_frame error: {e}");
                emit_error(&event_tx, SignalingError::Io(e.to_string()));
                return;
            }
        }

        // Read one inbound frame. read_frame_or_pending returns Ok(None) when
        // no data is available (so the caller can drain inbox + check stop),
        // and retries internally on transient TimedOut/WouldBlock once a frame
        // has begun arriving — preventing the buffer-desync bug where
        // BufReader::read_exact silently consumes partial body bytes on
        // timeout, causing subsequent reads to start mid-frame.
        match read_frame_or_pending(&mut reader, &stop) {
            Ok(Some(frame)) => {
                let kind = match &frame {
                    SignalingFrame::Hello { proto } => format!("Hello (proto={proto})"),
                    SignalingFrame::Offer { sdp } => format!("Offer (sdp={} bytes)", sdp.len()),
                    SignalingFrame::Answer { sdp } => format!("Answer (sdp={} bytes)", sdp.len()),
                    SignalingFrame::Candidate { sdp } => {
                        format!("Candidate (sdp={} bytes)", sdp.len())
                    }
                    SignalingFrame::Bye => "Bye".to_string(),
                    SignalingFrame::ReconnectRequest {
                        attempt,
                        session_nonce,
                        ..
                    } => format!("ReconnectRequest (attempt={attempt}, nonce={session_nonce})"),
                    SignalingFrame::ReconnectAck {
                        attempt,
                        session_nonce,
                    } => format!("ReconnectAck (attempt={attempt}, nonce={session_nonce})"),
                };
                eprintln!("[sm-signaling-frame-loop] IN  ← {kind}");
                match frame_to_event(frame, &supervisor_signal_tx) {
                    Some(SignalingEvent::Closed) => {
                        eprintln!("[sm-signaling-frame-loop] EXIT: peer sent Bye → emit Closed");
                        let _ = emit(&event_tx, SignalingEvent::Closed);
                        break;
                    }
                    Some(ev) => {
                        let _ = emit(&event_tx, ev);
                    }
                    None => {} // Hello or reconnect frame — absorbed / routed to supervisor
                }
            }
            Ok(None) => {
                // No data available right now — loop to drain inbox + re-check stop.
                continue;
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                eprintln!("[sm-signaling-frame-loop] EXIT: peer closed (EOF) → emit Closed");
                let _ = emit(&event_tx, SignalingEvent::Closed);
                break;
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {
                // Stop flag was set during a partial read.
                eprintln!("[sm-signaling-frame-loop] EXIT: stop flag set during partial read");
                break;
            }
            Err(e) => {
                // Diagnostic: dump up to 64 more bytes from the reader so we can
                // see what content surrounded the oversize length prefix. This
                // helps identify whether we're mid-SDP-body, mid-JSON, or
                // looking at a foreign protocol payload.
                use std::io::BufRead;
                let extra: Vec<u8> = reader
                    .fill_buf()
                    .map(|b| b[..b.len().min(64)].to_vec())
                    .unwrap_or_default();
                let hex: String = extra
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                let ascii: String = extra
                    .iter()
                    .map(|b| {
                        if (0x20..0x7f).contains(b) {
                            *b as char
                        } else {
                            '.'
                        }
                    })
                    .collect();
                eprintln!("[sm-signaling-frame-loop] EXIT: read error: {e}");
                eprintln!(
                    "[sm-signaling-frame-loop] read error context: peer={peer} local={local}"
                );
                eprintln!(
                    "[sm-signaling-frame-loop] next {} bytes (hex)  : {hex}",
                    extra.len()
                );
                eprintln!(
                    "[sm-signaling-frame-loop] next {} bytes (ascii): {ascii}",
                    extra.len()
                );
                emit_error(
                    &event_tx,
                    SignalingError::Protocol(format!("frame read error: {e}")),
                );
                break;
            }
        }
    }
}

// ─── Resilient frame reader ──────────────────────────────────────────────────
//
// The TCP stream has a short read timeout (READ_TIMEOUT) so the frame loop
// can periodically check the stop flag and drain the outbound inbox. The
// previous implementation called wire::read_frame, which uses
// `Read::read_exact` internally. read_exact has the bad property that on
// TimedOut it CONSUMES partial bytes from the BufReader and discards them;
// the caller can only re-call read_exact with a fresh buffer, so any bytes
// already consumed are lost from the wire. For small frames (Hello = 33
// bytes, almost always a single TCP segment) this rarely fires, but for the
// SDP answer/offer (~4.6 KB across multiple segments with inter-segment
// gaps) the timeout commonly fires partway through the body, leaving the
// reader pointed mid-body. The next loop iteration's read_frame then reads
// 4 mid-SDP bytes as a length prefix and trips MAX_FRAME_BYTES — the
// "frame too large" / "na=r" symptom seen in B11 smoke.
//
// `read_frame_or_pending` fixes this by:
// - Returning Ok(None) when NO bytes have been consumed yet and the read
//   times out, so the loop keeps draining the inbox and checking stop.
// - Once any byte has been consumed, retrying transparently on
//   TimedOut/WouldBlock until the full prefix and body are read, ensuring
//   the wire stays aligned. Stop flag is checked between retries to keep
//   the loop interruptible.

/// Read a complete signaling frame, or return `Ok(None)` if no data is
/// currently available on the reader. Internally retries on TimedOut /
/// WouldBlock once a partial read has begun.
fn read_frame_or_pending<R: Read>(
    reader: &mut R,
    stop: &Arc<AtomicBool>,
) -> io::Result<Option<SignalingFrame>> {
    let mut prefix = [0u8; 4];
    // First-byte probe: if no data is available, return Ok(None) so the
    // caller can drain inbox + check stop. Once we read at least 1 byte we
    // are committed to completing this frame.
    match reader.read(&mut prefix[..1]) {
        Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "peer closed")),
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::TimedOut || e.kind() == io::ErrorKind::WouldBlock => {
            return Ok(None);
        }
        Err(e) => return Err(e),
    }

    // Complete the prefix with retry-on-timeout.
    read_exact_resilient(reader, &mut prefix[1..], stop)?;
    let len = u32::from_be_bytes(prefix) as usize;
    if len > MAX_FRAME_BYTES {
        let ascii: String = prefix
            .iter()
            .map(|b| {
                if (0x20..0x7f).contains(b) {
                    *b as char
                } else {
                    '.'
                }
            })
            .collect();
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "frame too large: declared {len} bytes (max {MAX_FRAME_BYTES}); raw prefix bytes: {:02x} {:02x} {:02x} {:02x} (\"{ascii}\")",
                prefix[0], prefix[1], prefix[2], prefix[3]
            ),
        ));
    }

    // Read the full body, tolerating transient timeouts.
    let mut body = vec![0u8; len];
    read_exact_resilient(reader, &mut body, stop)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Like `Read::read_exact`, but retries on TimedOut / WouldBlock and
/// honours `stop` (returning `Interrupted`) between attempts. Bytes
/// consumed across timeouts are accumulated in `buf`, so callers never
/// observe lost-byte desync.
fn read_exact_resilient<R: Read>(
    reader: &mut R,
    buf: &mut [u8],
    stop: &Arc<AtomicBool>,
) -> io::Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        if stop.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "stop flag set during read",
            ));
        }
        match reader.read(&mut buf[filled..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "peer closed mid-frame",
                ));
            }
            Ok(n) => filled += n,
            Err(e)
                if e.kind() == io::ErrorKind::TimedOut || e.kind() == io::ErrorKind::WouldBlock =>
            {
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Enumerate non-loopback IPv4 addresses on all active network interfaces.
fn collect_ipv4_addrs() -> Vec<Ipv4Addr> {
    match if_addrs::get_if_addrs() {
        Ok(ifaces) => ifaces
            .into_iter()
            .filter_map(|iface| {
                if let if_addrs::IfAddr::V4(v4) = iface.addr {
                    if !v4.ip.is_loopback() {
                        Some(v4.ip)
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect(),
        Err(_) => vec![],
    }
}

/// Emit a `SignalingEvent` on `event_tx` without blocking (drop-newest on full).
fn emit(tx: &SyncSender<SignalingEvent>, event: SignalingEvent) -> Result<(), ()> {
    match tx.try_send(event) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(_)) => Ok(()), // drop silently — channel is full
        Err(TrySendError::Disconnected(_)) => Err(()),
    }
}

/// Emit a `SignalingEvent::Error` wrapping the given `SignalingError`.
fn emit_error(tx: &SyncSender<SignalingEvent>, err: SignalingError) {
    let _ = emit(tx, SignalingEvent::Error(err));
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::mpsc::sync_channel;

    use sm_domain::signaling::{
        SdpOffer, Signaling, SignalingConfig, SignalingError, SignalingEvent, SignalingRole,
    };

    use super::bind_tcp_listener_reusable;
    use crate::signaling::mdns::MdnsSignaling;

    // ─── Compile-time check: implements Signaling ─────────────────────────────

    /// R7.1 — MdnsSignaling MUST implement Signaling (compile-time check).
    fn _assert_implements_signaling<T: Signaling>() {}
    fn _check() {
        _assert_implements_signaling::<MdnsSignaling>();
    }

    // ─── S7.1: mDNS discovery (ignored — requires multicast) ─────────────────

    /// S7.1 — Given two MdnsSignaling instances (sender + receiver) on the same host
    /// with a working multicast interface, when start() is called on both, then within
    /// 5 seconds each emits `SignalingEvent::PeerFound`.
    ///
    /// This test is `#[ignore]` per R7.5 — it requires mDNS multicast.
    /// Run manually: `cargo nextest run -- --run-ignored mdns_peer_discovery`
    #[test]
    #[ignore]
    fn mdns_peer_discovery_s7_1() {
        use std::time::Duration;

        let sender_config = SignalingConfig {
            role: SignalingRole::Sender,
            control_port: 17891,
            ..Default::default()
        };
        let receiver_config = SignalingConfig {
            role: SignalingRole::Receiver,
            control_port: 17891,
            ..Default::default()
        };

        let mut sender_sig = MdnsSignaling::new(sender_config).unwrap();
        let mut receiver_sig = MdnsSignaling::new(receiver_config).unwrap();

        let (s_tx, s_rx) = sync_channel::<SignalingEvent>(8);
        let (r_tx, r_rx) = sync_channel::<SignalingEvent>(8);

        sender_sig.start(s_tx).unwrap();
        receiver_sig.start(r_tx).unwrap();

        let timeout = Duration::from_secs(5);
        let sender_found = s_rx
            .recv_timeout(timeout)
            .map(|e| matches!(e, SignalingEvent::PeerFound { .. }))
            .unwrap_or(false);
        let receiver_found = r_rx
            .recv_timeout(timeout)
            .map(|e| matches!(e, SignalingEvent::PeerFound { .. }))
            .unwrap_or(false);

        sender_sig.stop().unwrap();
        receiver_sig.stop().unwrap();

        assert!(
            sender_found,
            "sender must emit PeerFound within 5 s (requires multicast)"
        );
        assert!(
            receiver_found,
            "receiver must emit PeerFound within 5 s (requires multicast)"
        );
    }

    // ─── frame_to_event mapping (unit, no network) ───────────────────────────

    use std::sync::Mutex;
    use std::sync::mpsc::sync_channel as sc;

    fn no_supervisor() -> std::sync::Arc<
        Mutex<Option<std::sync::mpsc::SyncSender<sm_domain::supervisor::SupervisorSignal>>>,
    > {
        std::sync::Arc::new(Mutex::new(None))
    }

    /// S7.2 — frame_to_event maps Offer frame to OfferReceived.
    #[test]
    fn frame_to_event_offer_maps_correctly() {
        use crate::signaling::mdns::frame_to_event;
        use crate::signaling::wire::SignalingFrame;

        let frame = SignalingFrame::Offer {
            sdp: "v=0".to_string(),
        };
        let event = frame_to_event(frame, &no_supervisor()).expect("Offer must produce an event");
        assert!(
            matches!(event, SignalingEvent::OfferReceived(SdpOffer(ref s)) if s == "v=0"),
            "Offer frame must map to OfferReceived with exact SDP"
        );
    }

    /// S7.2 — frame_to_event maps Answer frame to AnswerReceived.
    #[test]
    fn frame_to_event_answer_maps_correctly() {
        use crate::signaling::mdns::frame_to_event;
        use crate::signaling::wire::SignalingFrame;

        let frame = SignalingFrame::Answer {
            sdp: "v=0\r\nm=video".to_string(),
        };
        let event = frame_to_event(frame, &no_supervisor()).expect("Answer must produce an event");
        assert!(matches!(event, SignalingEvent::AnswerReceived(_)));
    }

    /// S7.2 — frame_to_event maps Candidate frame to CandidateReceived.
    #[test]
    fn frame_to_event_candidate_maps_correctly() {
        use crate::signaling::mdns::frame_to_event;
        use crate::signaling::wire::SignalingFrame;

        let frame = SignalingFrame::Candidate {
            sdp: "candidate:1 1 udp 2130706431 127.0.0.1 9 typ host".to_string(),
        };
        let event =
            frame_to_event(frame, &no_supervisor()).expect("Candidate must produce an event");
        assert!(matches!(event, SignalingEvent::CandidateReceived(_)));
    }

    /// S7.3 — Hello frame returns None (absorbed silently).
    #[test]
    fn frame_to_event_hello_returns_none() {
        use crate::signaling::mdns::frame_to_event;
        use crate::signaling::wire::SignalingFrame;

        let event = frame_to_event(
            SignalingFrame::Hello {
                proto: "v1".to_string(),
            },
            &no_supervisor(),
        );
        assert!(
            event.is_none(),
            "Hello frame must not produce a SignalingEvent"
        );
    }

    /// S7.3 — Bye frame produces Closed event.
    #[test]
    fn frame_to_event_bye_returns_closed() {
        use crate::signaling::mdns::frame_to_event;
        use crate::signaling::wire::SignalingFrame;

        let event =
            frame_to_event(SignalingFrame::Bye, &no_supervisor()).expect("Bye must produce Closed");
        assert!(
            matches!(event, SignalingEvent::Closed),
            "Bye frame must map to SignalingEvent::Closed"
        );
    }

    // ─── T5.1: frame_to_event routes reconnect frames to supervisor channel ──

    /// T5.1 / AC-5 — `ReconnectRequest` frame is routed to the supervisor channel
    /// as `SupervisorSignal::PeerRequest` when a supervisor_signal_tx is registered.
    #[test]
    fn frame_to_event_reconnect_request_routes_to_supervisor_channel() {
        use crate::signaling::mdns::frame_to_event;
        use crate::signaling::wire::SignalingFrame;
        use sm_domain::supervisor::SupervisorSignal;
        use std::time::Duration;

        let (sup_tx, sup_rx) = sc::<SupervisorSignal>(8);
        let supervisor_signal_tx = std::sync::Arc::new(Mutex::new(Some(sup_tx)));

        let frame = SignalingFrame::ReconnectRequest {
            attempt: 2,
            requester_role: SignalingRole::Sender,
            session_nonce: 42_000,
        };

        let result = frame_to_event(frame, &supervisor_signal_tx);
        assert!(
            result.is_none(),
            "ReconnectRequest must not produce a SignalingEvent (routed to supervisor)"
        );

        let signal = sup_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("supervisor channel must receive PeerRequest within 100ms");
        assert_eq!(
            signal,
            SupervisorSignal::PeerRequest {
                peer_nonce: 42_000,
                peer_role: SignalingRole::Sender,
                attempt: 2,
            }
        );
    }

    /// T5.1 / AC-6 — `ReconnectAck` frame is routed to the supervisor channel
    /// as `SupervisorSignal::PeerAck` when a supervisor_signal_tx is registered.
    #[test]
    fn frame_to_event_reconnect_ack_routes_to_supervisor_channel() {
        use crate::signaling::mdns::frame_to_event;
        use crate::signaling::wire::SignalingFrame;
        use sm_domain::supervisor::SupervisorSignal;
        use std::time::Duration;

        let (sup_tx, sup_rx) = sc::<SupervisorSignal>(8);
        let supervisor_signal_tx = std::sync::Arc::new(Mutex::new(Some(sup_tx)));

        let frame = SignalingFrame::ReconnectAck {
            attempt: 1,
            session_nonce: 99,
        };

        let result = frame_to_event(frame, &supervisor_signal_tx);
        assert!(
            result.is_none(),
            "ReconnectAck must not produce a SignalingEvent (routed to supervisor)"
        );

        let signal = sup_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("supervisor channel must receive PeerAck within 100ms");
        assert_eq!(
            signal,
            SupervisorSignal::PeerAck {
                session_nonce: 99,
                attempt: 1,
            }
        );
    }

    /// SC-DR-5 — `frame_to_event` MUST forward the wire `requester_role` into the
    /// `SupervisorSignal::PeerRequest { peer_role }` instead of discarding it.
    ///
    /// Root cause #962: `mdns.rs` dropped `requester_role` (`requester_role: _`),
    /// so the supervisor's tie-break was role-blind. Design #963 D1 plumbs the role
    /// through. This pins the plumbing: a `Receiver`-role request must arrive as
    /// `peer_role: Receiver` on the supervisor channel.
    #[test]
    fn sc_dr_5_requester_role_not_discarded() {
        use crate::signaling::mdns::frame_to_event;
        use crate::signaling::wire::SignalingFrame;
        use sm_domain::supervisor::SupervisorSignal;
        use std::time::Duration;

        let (sup_tx, sup_rx) = sc::<SupervisorSignal>(8);
        let supervisor_signal_tx = std::sync::Arc::new(Mutex::new(Some(sup_tx)));

        let frame = SignalingFrame::ReconnectRequest {
            attempt: 1,
            requester_role: SignalingRole::Receiver,
            session_nonce: 42,
        };

        let result = frame_to_event(frame, &supervisor_signal_tx);
        assert!(result.is_none(), "ReconnectRequest must not produce a SignalingEvent");

        let signal = sup_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("supervisor channel must receive PeerRequest within 100ms");
        assert_eq!(
            signal,
            SupervisorSignal::PeerRequest {
                peer_nonce: 42,
                peer_role: SignalingRole::Receiver,
                attempt: 1,
            }
        );
    }

    /// T5.1 — `ReconnectRequest` returns None silently when no supervisor channel is set.
    #[test]
    fn frame_to_event_reconnect_request_returns_none_without_supervisor() {
        use crate::signaling::mdns::frame_to_event;
        use crate::signaling::wire::SignalingFrame;

        let frame = SignalingFrame::ReconnectRequest {
            attempt: 1,
            requester_role: SignalingRole::Receiver,
            session_nonce: 1234,
        };

        let result = frame_to_event(frame, &no_supervisor());
        assert!(
            result.is_none(),
            "ReconnectRequest must return None (no supervisor registered)"
        );
    }

    // ─── T5.1: publish_reconnect_request / publish_reconnect_ack API ─────────

    /// T5.1 — `publish_reconnect_request` returns NotRunning before start().
    #[test]
    fn publish_reconnect_request_before_start_returns_not_running() {
        let sig = MdnsSignaling::new(SignalingConfig::default()).unwrap();
        let result = sig.publish_reconnect_request(1, SignalingRole::Sender, 42);
        assert!(
            matches!(result, Err(SignalingError::NotRunning)),
            "publish_reconnect_request before start must return NotRunning, got {result:?}"
        );
    }

    /// T5.1 — `publish_reconnect_ack` returns NotRunning before start().
    #[test]
    fn publish_reconnect_ack_before_start_returns_not_running() {
        let sig = MdnsSignaling::new(SignalingConfig::default()).unwrap();
        let result = sig.publish_reconnect_ack(1, 42);
        assert!(
            matches!(result, Err(SignalingError::NotRunning)),
            "publish_reconnect_ack before start must return NotRunning, got {result:?}"
        );
    }

    // ─── T5.2: reset() rebuilds a fresh MdnsSignaling ────────────────────────

    /// T5.2 — `reset()` on a stopped instance returns a new instance in pre-start state.
    #[test]
    fn mdns_signaling_reset_returns_fresh_instance() {
        let sig = MdnsSignaling::new(SignalingConfig::default()).unwrap();
        let fresh = sig.reset().expect("reset must succeed");
        // Fresh instance must be in pre-start state: publish returns NotRunning.
        let result = fresh.publish_local_offer(SdpOffer("v=0".to_string()));
        assert!(
            matches!(result, Err(SignalingError::NotRunning)),
            "after reset(), new instance must be in pre-start state; got {result:?}"
        );
    }

    // ─── new() via Signaling trait ─────────────────────────────────────────────

    /// R7.1 — MdnsSignaling::new succeeds without network access.
    #[test]
    fn mdns_signaling_new_succeeds() {
        assert!(
            MdnsSignaling::new(SignalingConfig::default()).is_ok(),
            "new() must succeed without network"
        );
    }

    // ─── stop() is idempotent ─────────────────────────────────────────────────

    /// R7.4 — stop() is idempotent: second call returns Ok without panic.
    #[test]
    fn mdns_signaling_stop_is_idempotent() {
        let mut sig = MdnsSignaling::new(SignalingConfig::default()).unwrap();
        sig.stop().unwrap();
        sig.stop().unwrap();
    }

    // ─── publish_local_offer before start → NotRunning ────────────────────────

    /// R7.4 — publish_local_offer before start() returns Err(NotRunning).
    #[test]
    fn mdns_publish_before_start_returns_not_running() {
        let sig = MdnsSignaling::new(SignalingConfig::default()).unwrap();
        let result = sig.publish_local_offer(SdpOffer("v=0".to_string()));
        assert!(
            matches!(result, Err(SignalingError::NotRunning)),
            "publish before start must return NotRunning, got {result:?}"
        );
    }

    // ─── B11-S2 regression: resilient frame reader ──────────────────────────────

    use std::io::{self, Read};
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use crate::signaling::mdns::read_frame_or_pending;
    use crate::signaling::wire::{SignalingFrame, write_frame};

    /// A test reader that simulates a TCP stream with arbitrary partial-read
    /// + transient-timeout behaviour. Each entry in `script` is either:
    /// - `Ok(n)` — return up to `n` bytes from the pending buffer.
    /// - `Err(kind)` — return that error kind.
    enum Step {
        /// Deliver up to N bytes (or fewer if the buffer is shorter).
        Bytes(usize),
        /// Return a transient error (TimedOut or WouldBlock).
        Timeout,
        /// Return Ok(0) to signal EOF.
        Eof,
    }

    struct ScriptedReader {
        data: Vec<u8>,
        cursor: usize,
        steps: std::vec::IntoIter<Step>,
    }

    impl Read for ScriptedReader {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            match self.steps.next() {
                Some(Step::Bytes(n)) => {
                    let avail = self.data.len() - self.cursor;
                    let want = n.min(out.len()).min(avail);
                    out[..want].copy_from_slice(&self.data[self.cursor..self.cursor + want]);
                    self.cursor += want;
                    Ok(want)
                }
                Some(Step::Timeout) => {
                    Err(io::Error::new(io::ErrorKind::TimedOut, "scripted timeout"))
                }
                Some(Step::Eof) => Ok(0),
                None => Err(io::Error::other("script exhausted")),
            }
        }
    }

    /// B11-S2 regression: the body read MUST NOT lose bytes when the underlying
    /// reader returns transient TimedOut between segments. read_frame_or_pending
    /// should accumulate partial reads and parse the complete frame.
    #[test]
    fn read_frame_or_pending_survives_timeout_mid_body() {
        // Construct a real Answer frame on the wire.
        let frame = SignalingFrame::Answer {
            sdp: "v=0\r\no=test 1 1 IN IP4 0.0.0.0\r\na=rtcp-fb:127 nack pli\r\na=fmtp:127 level-asymmetry-allowed=1\r\n".to_string(),
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &frame).expect("write_frame must succeed");

        // Simulate: deliver prefix in two chunks (1 then 3), then body in
        // three chunks with TimedOut between each chunk.
        let body_len = buf.len() - 4;
        let third = body_len / 3;
        let steps = vec![
            Step::Bytes(1), // first byte of prefix
            Step::Bytes(3), // remaining 3 bytes of prefix
            Step::Bytes(third),
            Step::Timeout,
            Step::Bytes(third),
            Step::Timeout,
            Step::Bytes(body_len), // remainder
        ];
        let mut reader = ScriptedReader {
            data: buf,
            cursor: 0,
            steps: steps.into_iter(),
        };
        let stop = Arc::new(AtomicBool::new(false));

        let result = read_frame_or_pending(&mut reader, &stop)
            .expect("read must succeed despite mid-body timeouts")
            .expect("must return Some(frame)");

        match result {
            SignalingFrame::Answer { sdp } => assert!(
                sdp.contains("rtcp-fb:127 nack pli"),
                "frame body must round-trip without byte loss"
            ),
            other => panic!("expected Answer, got {other:?}"),
        }
    }

    /// First-byte timeout MUST return Ok(None) so the caller can drain the
    /// outbound inbox + check the stop flag, NOT consume any bytes.
    #[test]
    fn read_frame_or_pending_first_byte_timeout_returns_none() {
        let mut reader = ScriptedReader {
            data: Vec::new(),
            cursor: 0,
            steps: vec![Step::Timeout].into_iter(),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let result =
            read_frame_or_pending(&mut reader, &stop).expect("first-byte timeout must not error");
        assert!(
            result.is_none(),
            "first-byte timeout must return Ok(None), got {result:?}"
        );
    }

    /// Stop flag set during a partial read MUST surface as Interrupted.
    #[test]
    fn read_frame_or_pending_stop_flag_returns_interrupted() {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);
        // First read returns 1 byte (commits to the frame); we set stop
        // before the next attempt.
        let mut reader = ScriptedReader {
            data: vec![0u8; 4],
            cursor: 0,
            steps: vec![Step::Bytes(1), Step::Timeout, Step::Timeout].into_iter(),
        };
        // Set stop AFTER constructing reader. The retry loop will see it.
        stop_clone.store(true, std::sync::atomic::Ordering::Release);
        let err = read_frame_or_pending(&mut reader, &stop)
            .expect_err("must return Err when stop is set during retry");
        assert_eq!(
            err.kind(),
            io::ErrorKind::Interrupted,
            "stop flag must produce Interrupted, got {err:?}"
        );
    }

    /// Mid-frame EOF (peer closed after sending partial body) MUST surface
    /// as UnexpectedEof.
    #[test]
    fn read_frame_or_pending_mid_frame_eof_returns_unexpected_eof() {
        let frame = SignalingFrame::Hello {
            proto: "v1".to_string(),
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &frame).unwrap();
        // Truncate the body so EOF arrives mid-body.
        buf.truncate(buf.len() - 5);
        let mut reader = ScriptedReader {
            data: buf,
            cursor: 0,
            // First-byte then everything we have (which is short), then EOF.
            steps: vec![Step::Bytes(1), Step::Bytes(usize::MAX), Step::Eof].into_iter(),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let err =
            read_frame_or_pending(&mut reader, &stop).expect_err("must error on truncated frame");
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    // ─── SC-1: bind_tcp_listener_reusable — bind→drop→rebind (cross-platform) ─

    /// SC-1 — After dropping a TcpListener, the same port can be rebound immediately.
    /// Exercises TIME_WAIT rebind on Unix and confirms SO_REUSEADDR is set correctly
    /// on all platforms.
    #[test]
    fn bind_tcp_listener_reusable_rebind_after_drop_succeeds() {
        use std::net::SocketAddr;
        let zero: SocketAddr = "0.0.0.0:0".parse().unwrap();
        let listener1 = bind_tcp_listener_reusable(zero).expect("first bind");
        let port = listener1.local_addr().unwrap().port();
        drop(listener1);

        let fixed: SocketAddr = format!("0.0.0.0:{port}").parse().unwrap();
        let listener2 = bind_tcp_listener_reusable(fixed);
        assert!(
            listener2.is_ok(),
            "rebind after drop must succeed (got: {:?})",
            listener2.err()
        );
    }

    // ─── SC-3: bind_tcp_listener_reusable — live rebind (Windows-only) ────────

    /// SC-3 — On Windows, SO_REUSEADDR allows a second bind while the first socket
    /// is still alive (LISTEN state). This reproduces the Arc-lifetime race where the
    /// old MdnsSignaling thread still holds 0.0.0.0:7889 when a rebuild worker
    /// attempts to bind a new one (engram #1417).
    #[cfg(target_os = "windows")]
    #[test]
    fn bind_tcp_listener_reusable_live_rebind_windows_succeeds() {
        use std::net::SocketAddr;
        let zero: SocketAddr = "0.0.0.0:0".parse().unwrap();
        let listener1 = bind_tcp_listener_reusable(zero).expect("first bind");
        let port = listener1.local_addr().unwrap().port();
        let _hold = listener1; // intentionally NOT dropped — reproduces Arc race

        let fixed: SocketAddr = format!("0.0.0.0:{port}").parse().unwrap();
        let listener2 = bind_tcp_listener_reusable(fixed);
        assert!(
            listener2.is_ok(),
            "live rebind on Windows must succeed (Arc race repro), got: {:?}",
            listener2.err()
        );
    }

    // ─── SC-C-001: DISCOVER_TIMEOUT must be 30s ───────────────────────────────────

    /// SC-C-001 — `DISCOVER_TIMEOUT` constant must be `Duration::from_secs(30)`.
    ///
    /// RED: current value is `Duration::from_secs(10)` (too short for sender republish
    ///      after S-1 supervisor wakes — see design D-7). A 10s window guarantees a
    ///      miss in the legacy path and gives zero margin in the S-1 path.
    ///
    /// GREEN (T07): change constant to 30s → test passes.
    #[test]
    fn discover_timeout_is_30s() {
        use std::time::Duration;
        assert_eq!(
            super::DISCOVER_TIMEOUT,
            Duration::from_secs(30),
            "SC-C-001 FAIL: DISCOVER_TIMEOUT must be Duration::from_secs(30). \
             Current value is {:?}. \
             Fix (D-7): change `const DISCOVER_TIMEOUT: Duration = Duration::from_secs(10)` \
             to `Duration::from_secs(30)` in mdns.rs.",
            super::DISCOVER_TIMEOUT
        );
    }

    // ─── SC-D-001 / SC-D-002: mdns.shutdown() MUST be called AFTER run_frame_loop ──

    /// SC-D-001 — Sender thread: `mdns.shutdown()` must be called AFTER `run_frame_loop`
    /// returns.
    ///
    /// Test strategy: start a real `MdnsSignaling` sender, connect to it via loopback
    /// TCP (bypassing mDNS discovery), drive `run_frame_loop` to exit by closing the
    /// connection, then verify that `MdnsSignaling::stop()` completes cleanly AND
    /// that `SignalingEvent::Closed` was emitted by the frame loop (not before it).
    ///
    /// The KEY invariant: `SignalingEvent::Closed` is emitted by `run_frame_loop` when
    /// the peer closes the connection. With the BROKEN code, `mdns.shutdown()` is
    /// called BEFORE `run_frame_loop` starts — but the frame loop still runs (shutdown
    /// does not prevent TCP). The observable difference is that with the BROKEN code,
    /// the mDNS service entry is removed from the network BEFORE the TCP session ends,
    /// which means a reconnecting receiver would find no service during the session.
    ///
    /// For a unit test without real mDNS, we verify the SEQUENCING by tracking when
    /// `SignalingEvent::Closed` arrives relative to `stop()` returning. The frame loop
    /// MUST have run (producing Closed) before the signaling stop completes.
    ///
    /// RED (current code): mdns.shutdown() is at L495 BEFORE run_frame_loop at L496.
    ///     The test detects this via: `DISCOVER_TIMEOUT` constant value is 10s.
    ///     After D-8 fix: shutdown moves to after run_frame_loop.
    ///
    /// For a concrete RED/GREEN test that verifies the SOURCE CODE ordering without
    /// real mDNS infrastructure, we use a source-text structural assertion:
    /// the line "let _ = mdns.shutdown();" must appear AFTER the line
    /// "run_frame_loop(" in the sender thread function body.
    ///
    /// This is a legitimate static gate: any refactor that re-introduces the broken
    /// ordering will fail this test immediately, catching regressions at compile/test
    /// time on every CI run.
    #[test]
    fn sender_mdns_shutdown_happens_after_frame_loop() {
        // Read the source file (relative to the manifest directory at test time).
        // `CARGO_MANIFEST_DIR` is set by Cargo for all crate-level tests.
        let manifest_dir =
            std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set by Cargo");
        let source_path = std::path::PathBuf::from(&manifest_dir).join("src/signaling/mdns.rs");
        let source =
            std::fs::read_to_string(&source_path).expect("mdns.rs must be readable in tests");

        // Find run_sender_thread body boundaries.
        let sender_fn_start = source
            .find("fn run_sender_thread(")
            .expect("run_sender_thread must exist in mdns.rs");
        let sender_fn_end = {
            source[sender_fn_start..]
                .find("\nfn run_receiver_thread(")
                .map(|rel| sender_fn_start + rel)
                .unwrap_or(source.len())
        };
        let sender_body = &source[sender_fn_start..sender_fn_end];

        // Use rfind for the LAST occurrence of mdns.shutdown() in the sender body.
        // The early-exit shutdown calls (inside error branches) are NOT the main-path
        // shutdown. The main-path shutdown is the one directly adjacent to run_frame_loop.
        // rfind finds the LAST occurrence which should be the main-path one.
        let shutdown_pos = sender_body
            .rfind("mdns.shutdown()")
            .expect("mdns.shutdown() must appear in run_sender_thread");
        let frame_loop_pos = sender_body
            .rfind("run_frame_loop(")
            .expect("run_frame_loop( must appear in run_sender_thread");

        // SC-D-001 assertion: the main-path mdns.shutdown() MUST appear AFTER run_frame_loop.
        // RED: current code has shutdown at L495 BEFORE run_frame_loop at L496.
        //      With the early-exit shutdowns present, rfind still finds the main-path
        //      shutdown. In BROKEN state, the main-path shutdown is BEFORE run_frame_loop.
        // GREEN (T05): main-path shutdown moved to AFTER run_frame_loop.
        assert!(
            shutdown_pos > frame_loop_pos,
            "SC-D-001 FAIL: in run_sender_thread, the main-path mdns.shutdown() \
             (last occurrence, byte offset {shutdown_pos}) appears BEFORE run_frame_loop \
             (last occurrence, byte offset {frame_loop_pos}). \
             Fix (D-8): move `let _ = mdns.shutdown();` to AFTER `run_frame_loop(...)` \
             so the mDNS service stays published during the entire TCP session."
        );
    }

    /// SC-D-002 — Receiver thread: `mdns.shutdown()` must be called AFTER `run_frame_loop`
    /// returns.
    ///
    /// Mirror of SC-D-001 for `run_receiver_thread`.
    ///
    /// RED: production code has `mdns.shutdown()` at L574 BEFORE `run_frame_loop` at L584.
    /// GREEN (T05): shutdown moved to after `run_frame_loop`.
    #[test]
    fn receiver_mdns_shutdown_happens_after_frame_loop() {
        let manifest_dir =
            std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set by Cargo");
        let source_path = std::path::PathBuf::from(&manifest_dir).join("src/signaling/mdns.rs");
        let source =
            std::fs::read_to_string(&source_path).expect("mdns.rs must be readable in tests");

        // Find run_receiver_thread body boundaries.
        let receiver_fn_start = source
            .find("fn run_receiver_thread(")
            .expect("run_receiver_thread must exist in mdns.rs");
        let receiver_fn_end = {
            source[receiver_fn_start..]
                .find("\n// ─── Shared TCP frame loop")
                .map(|rel| receiver_fn_start + rel)
                .unwrap_or(source.len())
        };
        let receiver_body = &source[receiver_fn_start..receiver_fn_end];

        // Use rfind: the main-path shutdown is the LAST occurrence (after run_frame_loop).
        let shutdown_pos = receiver_body
            .rfind("mdns.shutdown()")
            .expect("mdns.shutdown() must appear in run_receiver_thread");
        let frame_loop_pos = receiver_body
            .rfind("run_frame_loop(")
            .expect("run_frame_loop( must appear in run_receiver_thread");

        // SC-D-002 assertion: main-path mdns.shutdown() MUST appear AFTER run_frame_loop.
        // RED: current code has shutdown at L574 BEFORE run_frame_loop at L584.
        // GREEN (T05): shutdown moved to AFTER run_frame_loop.
        assert!(
            shutdown_pos > frame_loop_pos,
            "SC-D-002 FAIL: in run_receiver_thread, the main-path mdns.shutdown() \
             (last occurrence, byte offset {shutdown_pos}) appears BEFORE run_frame_loop \
             (last occurrence, byte offset {frame_loop_pos}). \
             Fix (D-8): move `let _ = mdns.shutdown();` to AFTER `run_frame_loop(...)` \
             so the mDNS service stays published during the entire TCP session."
        );
    }

    // ─── SC-S1-001 (mdns): frame_to_event(Bye) sends LocalFailure{PeerBye} ────
    //
    // REQ-S1 / D-5: frame_to_event(Bye) MUST send SupervisorSignal::LocalFailure
    // { trigger: PeerBye } to supervisor_signal_tx when it is Some(_), IN ADDITION
    // to returning Some(SignalingEvent::Closed).
    //
    // RED: currently `SignalingFrame::Bye => Some(SignalingEvent::Closed)` — no send.
    // GREEN (T13): Bye-arm patched to try_send LocalFailure{PeerBye} first.

    /// SC-S1-001 (mdns) — `frame_to_event(Bye)` sends `LocalFailure{PeerBye}` to
    ///                     `supervisor_signal_tx` when `Some(_)`, AND returns `Some(Closed)`.
    ///
    /// GIVEN: A `supervisor_signal_tx: Arc<Mutex<Option<SyncSender<SupervisorSignal>>>>`
    ///        pre-populated with `Some(sup_tx)`.
    /// WHEN:  `frame_to_event(SignalingFrame::Bye, &supervisor_signal_tx)` is called.
    /// THEN:  1. Returns `Some(SignalingEvent::Closed)` (existing behavior preserved).
    ///        2. `sup_rx` receives `SupervisorSignal::LocalFailure { trigger: PeerBye }`
    ///           within 100ms (NEW: S-1 eager wake).
    #[test]
    fn sc_s1_001_frame_to_event_bye_sends_local_failure_peer_bye_to_supervisor() {
        use crate::signaling::mdns::frame_to_event;
        use crate::signaling::wire::SignalingFrame;
        use sm_domain::session::ReconnectTrigger;
        use sm_domain::supervisor::SupervisorSignal;
        use std::sync::mpsc::sync_channel;
        use std::sync::{Arc, Mutex};
        use std::time::Duration;

        // ── Wire supervisor_signal_tx with Some(sup_tx) ──────────────────────
        let (sup_tx, sup_rx) = sync_channel::<SupervisorSignal>(8);
        let supervisor_signal_tx = Arc::new(Mutex::new(Some(sup_tx)));

        // ── WHEN: call frame_to_event(Bye) ───────────────────────────────────
        // RED: currently returns Closed but does NOT send to supervisor.
        let event = frame_to_event(SignalingFrame::Bye, &supervisor_signal_tx);

        // ── Assertion 1: still returns Some(Closed) ─────────────────────────
        assert!(
            matches!(event, Some(sm_domain::signaling::SignalingEvent::Closed)),
            "SC-S1-001: frame_to_event(Bye) must still return Some(Closed)"
        );

        // ── Assertion 2: supervisor receives LocalFailure{PeerBye} ────────────
        // RED: this fails until mdns.rs Bye-arm is patched (T13 GREEN).
        let signal = sup_rx.recv_timeout(Duration::from_millis(100)).expect(
            "SC-S1-001: frame_to_event(Bye) must send LocalFailure{PeerBye} to \
                 supervisor_signal_tx when Some(_) — RED until mdns.rs Bye-arm patched",
        );

        assert!(
            matches!(
                signal,
                SupervisorSignal::LocalFailure {
                    trigger: ReconnectTrigger::PeerBye
                }
            ),
            "SC-S1-001: expected LocalFailure{{PeerBye}} but got {signal:?}"
        );
    }

    /// SC-S1-001b — `frame_to_event(Bye)` with `None` supervisor does NOT panic.
    ///
    /// When `supervisor_signal_tx` is `None`, the Bye-arm must silently skip the send.
    /// Returns `Some(Closed)` as before.
    #[test]
    fn sc_s1_001b_frame_to_event_bye_with_none_supervisor_returns_closed() {
        use crate::signaling::mdns::frame_to_event;
        use crate::signaling::wire::SignalingFrame;

        let event = frame_to_event(SignalingFrame::Bye, &no_supervisor());
        assert!(
            matches!(event, Some(sm_domain::signaling::SignalingEvent::Closed)),
            "SC-S1-001b: frame_to_event(Bye) with None supervisor must return Some(Closed)"
        );
    }
}
