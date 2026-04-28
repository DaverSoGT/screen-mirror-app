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

use crate::signaling::wire::{MAX_FRAME_BYTES, SignalingFrame, write_frame};

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
                };
                eprintln!("[sm-signaling-frame-loop] IN  ← {kind}");
                match frame_to_event(frame) {
                    Some(SignalingEvent::Closed) => {
                        eprintln!("[sm-signaling-frame-loop] EXIT: peer sent Bye → emit Closed");
                        let _ = emit(&event_tx, SignalingEvent::Closed);
                        break;
                    }
                    Some(ev) => {
                        let _ = emit(&event_tx, ev);
                    }
                    None => {} // Hello — absorbed silently
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
}
