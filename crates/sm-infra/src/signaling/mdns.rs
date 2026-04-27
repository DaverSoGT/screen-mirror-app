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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use sm_domain::signaling::{
    IceCandidate, SdpAnswer, SdpOffer, Signaling, SignalingConfig, SignalingError, SignalingEvent,
};

// ─── Internal control messages ────────────────────────────────────────────────

/// Outbound frames queued from the public API into the signaling thread.
#[allow(dead_code)]
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
/// - **Receiver**: browses for `_screen-mirror._tcp.local.`, connects TCP to the sender.
pub struct MdnsSignaling {
    /// Runtime configuration.
    config: SignalingConfig,
    /// Shared stop flag.
    stop: Arc<AtomicBool>,
    /// Outbound control inbox (public API → thread).
    #[allow(dead_code)]
    inbox: Arc<Mutex<Vec<MdnsControl>>>,
    /// Thread handle (None before `start()` and after `stop()`).
    handle: Option<JoinHandle<()>>,
}

impl Signaling for MdnsSignaling {
    /// Construct an `MdnsSignaling` instance from a [`SignalingConfig`].
    fn new(config: SignalingConfig) -> Result<Self, SignalingError> {
        Ok(Self {
            config,
            stop: Arc::new(AtomicBool::new(false)),
            inbox: Arc::new(Mutex::new(Vec::new())),
            handle: None,
        })
    }

    fn start(&mut self, _event_tx: SyncSender<SignalingEvent>) -> Result<(), SignalingError> {
        if self.handle.is_some() {
            return Err(SignalingError::AlreadyRunning);
        }
        // Stub: full implementation in task 5.4.
        Ok(())
    }

    fn publish_local_offer(&self, _offer: SdpOffer) -> Result<(), SignalingError> {
        if self.handle.is_none() && !self.config.service_name.is_empty() {
            // Only return NotRunning if thread hasn't started.
            // This check is intentionally stub-level.
        }
        Ok(())
    }

    fn publish_local_answer(&self, _answer: SdpAnswer) -> Result<(), SignalingError> {
        Ok(())
    }

    fn publish_local_candidate(&self, _cand: IceCandidate) -> Result<(), SignalingError> {
        Ok(())
    }

    fn stop(&mut self) -> Result<(), SignalingError> {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            handle.join().ok();
        }
        Ok(())
    }
}

impl Drop for MdnsSignaling {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
