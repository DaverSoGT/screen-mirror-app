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

use std::io::{BufReader, BufWriter};
use std::net::{IpAddr, Ipv4Addr, TcpListener, TcpStream};
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

use crate::signaling::wire::{SignalingFrame, read_frame, write_frame};

// ─── Constants ───────────────────────────────────────────────────────────────

/// mDNS service type for screen-mirror.
const SERVICE_TYPE: &str = "_screen-mirror._tcp.local.";

/// Instance name used when registering the sender service.
const INSTANCE_NAME: &str = "screen-mirror";

/// mDNS/TCP discovery timeout before `PeerNotFound` is reported.
const DISCOVER_TIMEOUT: Duration = Duration::from_secs(10);

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
}

impl Signaling for MdnsSignaling {
    /// Construct an `MdnsSignaling` instance. No threads started, no network touched.
    fn new(config: SignalingConfig) -> Result<Self, SignalingError> {
        Ok(Self {
            config,
            stop: Arc::new(AtomicBool::new(false)),
            inbox: Arc::new(Mutex::new(Vec::new())),
            handle: None,
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

        let handle = thread::Builder::new()
            .name("sm-signaling-mdns".to_string())
            .spawn(move || {
                run_signaling_thread(config, stop, inbox, event_tx);
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
/// All other variants map 1-to-1 to `SignalingEvent`.
pub(crate) fn frame_to_event(frame: SignalingFrame) -> Option<SignalingEvent> {
    match frame {
        SignalingFrame::Hello { proto: _ } => None,
        SignalingFrame::Offer { sdp } => Some(SignalingEvent::OfferReceived(SdpOffer(sdp))),
        SignalingFrame::Answer { sdp } => Some(SignalingEvent::AnswerReceived(SdpAnswer(sdp))),
        SignalingFrame::Candidate { sdp } => {
            Some(SignalingEvent::CandidateReceived(IceCandidate(sdp)))
        }
        SignalingFrame::Bye => Some(SignalingEvent::Closed),
    }
}

// ─── Thread entry point ───────────────────────────────────────────────────────

/// Dispatch to the sender or receiver thread based on role.
fn run_signaling_thread(
    config: SignalingConfig,
    stop: Arc<AtomicBool>,
    inbox: Arc<Mutex<Vec<MdnsControl>>>,
    event_tx: SyncSender<SignalingEvent>,
) {
    match config.role {
        SignalingRole::Sender => run_sender_thread(config, stop, inbox, event_tx),
        SignalingRole::Receiver => run_receiver_thread(config, stop, inbox, event_tx),
    }
}

// ─── Sender thread ────────────────────────────────────────────────────────────

fn run_sender_thread(
    config: SignalingConfig,
    stop: Arc<AtomicBool>,
    inbox: Arc<Mutex<Vec<MdnsControl>>>,
    event_tx: SyncSender<SignalingEvent>,
) {
    let port = config.control_port;

    // Bind TCP listener BEFORE mDNS registration so the receiver can connect
    // immediately after discovery.
    let listener = match TcpListener::bind(format!("0.0.0.0:{port}")) {
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

    let _ = mdns.shutdown();
    run_frame_loop(stream, stop, inbox, event_tx);
}

// ─── Receiver thread ──────────────────────────────────────────────────────────

fn run_receiver_thread(
    _config: SignalingConfig,
    stop: Arc<AtomicBool>,
    inbox: Arc<Mutex<Vec<MdnsControl>>>,
    event_tx: SyncSender<SignalingEvent>,
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
    let _ = mdns.shutdown();

    let stream = match TcpStream::connect((peer_ip, peer_port)) {
        Ok(s) => s,
        Err(e) => {
            emit_error(&event_tx, SignalingError::Io(e.to_string()));
            return;
        }
    };

    run_frame_loop(stream, stop, inbox, event_tx);
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
) {
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

    let mut writer = BufWriter::new(write_stream);
    let mut reader = BufReader::new(stream);

    // Send Hello frame first.
    if let Err(e) = write_frame(
        &mut writer,
        &SignalingFrame::Hello {
            proto: "v1".to_string(),
        },
    ) {
        emit_error(&event_tx, SignalingError::Io(e.to_string()));
        return;
    }

    loop {
        if stop.load(Ordering::Acquire) {
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
            };
            let kind = match &frame {
                SignalingFrame::Offer { sdp } => format!("Offer (sdp={} bytes)", sdp.len()),
                SignalingFrame::Answer { sdp } => format!("Answer (sdp={} bytes)", sdp.len()),
                SignalingFrame::Candidate { sdp } => format!("Candidate (sdp={} bytes)", sdp.len()),
                SignalingFrame::Hello { proto } => format!("Hello (proto={proto})"),
                SignalingFrame::Bye => "Bye".to_string(),
            };
            eprintln!("[sm-signaling-frame-loop] OUT → {kind}");
            if let Err(e) = write_frame(&mut writer, &frame) {
                eprintln!("[sm-signaling-frame-loop] write_frame error: {e}");
                emit_error(&event_tx, SignalingError::Io(e.to_string()));
                return;
            }
        }

        // Read one inbound frame (with timeout).
        match read_frame(&mut reader) {
            Ok(frame) => {
                let kind = match &frame {
                    SignalingFrame::Hello { proto } => format!("Hello (proto={proto})"),
                    SignalingFrame::Offer { sdp } => format!("Offer (sdp={} bytes)", sdp.len()),
                    SignalingFrame::Answer { sdp } => format!("Answer (sdp={} bytes)", sdp.len()),
                    SignalingFrame::Candidate { sdp } => format!("Candidate (sdp={} bytes)", sdp.len()),
                    SignalingFrame::Bye => "Bye".to_string(),
                };
                eprintln!("[sm-signaling-frame-loop] IN  ← {kind}");
                match frame_to_event(frame) {
                    Some(SignalingEvent::Closed) => {
                        let _ = emit(&event_tx, SignalingEvent::Closed);
                        break;
                    }
                    Some(ev) => {
                        let _ = emit(&event_tx, ev);
                    }
                    None => {} // Hello — absorbed silently
                }
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::TimedOut
                    || e.kind() == std::io::ErrorKind::WouldBlock =>
            {
                // Timeout — loop to re-check stop flag and drain inbox.
                continue;
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                let _ = emit(&event_tx, SignalingEvent::Closed);
                break;
            }
            Err(e) => {
                emit_error(
                    &event_tx,
                    SignalingError::Protocol(format!("frame read error: {e}")),
                );
                break;
            }
        }
    }
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

    /// S7.2 — frame_to_event maps Offer frame to OfferReceived.
    #[test]
    fn frame_to_event_offer_maps_correctly() {
        use crate::signaling::mdns::frame_to_event;
        use crate::signaling::wire::SignalingFrame;

        let frame = SignalingFrame::Offer {
            sdp: "v=0".to_string(),
        };
        let event = frame_to_event(frame).expect("Offer must produce an event");
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
        let event = frame_to_event(frame).expect("Answer must produce an event");
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
        let event = frame_to_event(frame).expect("Candidate must produce an event");
        assert!(matches!(event, SignalingEvent::CandidateReceived(_)));
    }

    /// S7.3 — Hello frame returns None (absorbed silently).
    #[test]
    fn frame_to_event_hello_returns_none() {
        use crate::signaling::mdns::frame_to_event;
        use crate::signaling::wire::SignalingFrame;

        let event = frame_to_event(SignalingFrame::Hello {
            proto: "v1".to_string(),
        });
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

        let event = frame_to_event(SignalingFrame::Bye).expect("Bye must produce Closed");
        assert!(
            matches!(event, SignalingEvent::Closed),
            "Bye frame must map to SignalingEvent::Closed"
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
}
