// Rebuild-generation independence tests (Batch 5, T5.1).
//
// Verifies that each new Str0mVideoSender generation starts with the ICE gate
// closed (ice_ready = false), independent of any prior generation's gate state.
//
// Design §D6: "rebuild starts ice_ready=false is a STRUCTURAL property of the
// call graph" — let mut ice_ready = false; is re-executed on every run_sender_loop
// call frame. These tests confirm the observable consequence: pre-ICE packets
// are dropped and counted in the new generation.
//
// These tests use only the public API of Str0mVideoSender (no pub(crate) seams),
// observing dropped_frames() before and after a simulated rebuild.

use std::sync::Arc;
use std::sync::mpsc::sync_channel;
use std::time::Duration;

use sm_domain::encode::{EncodedPacket, EncoderConfig, VideoEncoder};
use sm_domain::transport::{TransportConfig, TransportEvent, VideoSender};
use sm_infra::transport::Str0mVideoSender;

// ─── Minimal FakeEncoder ─────────────────────────────────────────────────────

struct FakeEncoder;

impl VideoEncoder for FakeEncoder {
    fn new(_config: EncoderConfig) -> Result<Self, sm_domain::encode::EncoderError>
    where
        Self: Sized,
    {
        Ok(Self)
    }

    fn start(
        &mut self,
        _rx: std::sync::mpsc::Receiver<sm_domain::encode::FramePayload>,
        _tx: std::sync::mpsc::SyncSender<EncodedPacket>,
    ) -> Result<(), sm_domain::encode::EncoderError> {
        Ok(())
    }

    fn stop(&mut self) -> Result<(), sm_domain::encode::EncoderError> {
        Ok(())
    }

    fn request_keyframe(&self) {}

    fn set_bitrate(&self, _bps: u32) -> Result<(), sm_domain::encode::EncoderError> {
        Ok(())
    }

    fn dropped_frames(&self) -> u64 {
        0
    }

    fn backend_name(&self) -> &'static str {
        "sw_fake"
    }
}

fn make_packet(seq: u64) -> EncodedPacket {
    EncodedPacket {
        data: vec![0u8; 16].into(),
        timestamp: Duration::ZERO,
        is_keyframe: false,
        sequence: seq,
    }
}

// ─── T5.1: rebuild generation starts with ice gate closed ────────────────────

/// T5.1 (AC-5) — A new Str0mVideoSender generation (simulating a rebuild) starts
/// with ice_ready = false: pre-ICE packets are dropped and counted.
///
/// Structural property verified: each call to Str0mVideoSender::start() spawns a
/// fresh run_sender_loop call frame, re-initialising `let mut ice_ready = false`.
/// The prior generation's gate state cannot bleed into the new generation.
#[test]
fn rebuild_generation_starts_with_ice_gate_closed() {
    // ── Generation 1 ────────────────────────────────────────────────────────────

    let mut sender1 = Str0mVideoSender::new(TransportConfig {
        udp_port: 0,
        ..TransportConfig::default()
    })
    .unwrap();
    let enc1 = Arc::new(FakeEncoder) as Arc<dyn VideoEncoder + Send + Sync>;
    sender1.set_encoder(enc1);

    let (pkt_tx1, pkt_rx1) = sync_channel::<EncodedPacket>(8);
    let (ev_tx1, _ev_rx1) = sync_channel::<TransportEvent>(4);
    sender1.start(pkt_rx1, ev_tx1).unwrap();

    // Send 3 pre-ICE packets to generation 1. Gate is closed (ice_ready=false),
    // so they must be dropped and counted.
    for i in 0..3u64 {
        let _ = pkt_tx1.send(make_packet(i));
    }
    // 250ms > the 200ms max tick timeout: guarantees at least one full iteration.
    std::thread::sleep(Duration::from_millis(250));

    let gen1_dropped = sender1.dropped_frames();
    assert!(
        gen1_dropped >= 3,
        "gen1 dropped_frames must be >= 3 before ICE, got {gen1_dropped}"
    );

    // Stop generation 1 (simulates a rebuild teardown).
    drop(pkt_tx1);
    sender1.stop().unwrap();

    // ── Generation 2 ────────────────────────────────────────────────────────────
    // A fresh Str0mVideoSender — just like build_production_sender_bundle creates.
    // The new generation MUST start with ice_ready = false, independent of gen1.

    let mut sender2 = Str0mVideoSender::new(TransportConfig {
        udp_port: 0,
        ..TransportConfig::default()
    })
    .unwrap();
    let enc2 = Arc::new(FakeEncoder) as Arc<dyn VideoEncoder + Send + Sync>;
    sender2.set_encoder(enc2);

    let (pkt_tx2, pkt_rx2) = sync_channel::<EncodedPacket>(8);
    let (ev_tx2, _ev_rx2) = sync_channel::<TransportEvent>(4);
    sender2.start(pkt_rx2, ev_tx2).unwrap();

    // Send 3 pre-ICE packets to generation 2 BEFORE any ICE inject.
    // The gate must be closed in this new generation (ice_ready=false from fresh frame).
    for i in 0..3u64 {
        let _ = pkt_tx2.send(make_packet(i));
    }
    std::thread::sleep(Duration::from_millis(250));

    let gen2_dropped = sender2.dropped_frames();
    assert!(
        gen2_dropped >= 3,
        "gen2 dropped_frames must be >= 3 before ICE (gate must be closed in new generation), \
         got {gen2_dropped}"
    );

    drop(pkt_tx2);
    sender2.stop().unwrap();
}
