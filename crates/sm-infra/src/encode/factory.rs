#![cfg(target_os = "windows")]
//! Encoder factory: HW-first, SW-fallback.
//!
//! # Decision tree
//!
//! 1. If `SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER=1` → skip HW, go directly to SW.
//! 2. (with `hw-encoder` feature) Try `WindowsMftH264Encoder::new(config.clone())`:
//!    - `Ok(enc)` → return `Box<dyn VideoEncoder>` wrapping HW encoder.
//!    - `Err(InitFailed)` → log once, fall through to SW.
//!    - Any other `Err` → propagate immediately (configuration mistake).
//! 3. `WindowsOpenH264Encoder::new(config)` → return SW encoder.
//!
//! # Override
//!
//! Set `SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER=1` to bypass HW enumeration and
//! force the OpenH264 software path. Useful for debugging or CPU benchmarks.

use std::sync::Once;

use sm_domain::encode::{EncoderConfig, EncoderError, VideoEncoder};

use crate::encode::windows::WindowsOpenH264Encoder;

#[cfg(feature = "hw-encoder")]
use crate::encode::windows_mft::WindowsMftH264Encoder;

// ── SW-fallback log guard ─────────────────────────────────────────────────────

static SW_FALLBACK_LOGGED: Once = Once::new();

fn log_sw_fallback_once(reason: &str) {
    SW_FALLBACK_LOGGED.call_once(|| {
        tracing::info!(
            reason,
            "hardware H.264 MFT unavailable — falling back to software encoder (OpenH264)"
        );
    });
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Build the best available H.264 video encoder for this machine.
///
/// Attempts hardware (MFT) first; falls back to software (OpenH264) if
/// `InitFailed` is returned or if the `hw-encoder` Cargo feature is disabled.
///
/// Set `SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER=1` to skip HW enumeration.
///
/// Returns `Err(InvalidConfig)` if `config` is invalid — this error propagates
/// without trying the SW fallback (both encoders share the same validation).
pub fn build_video_encoder(
    config: EncoderConfig,
) -> Result<Box<dyn VideoEncoder + Send + Sync>, EncoderError> {
    build_video_encoder_with(config, hw_constructor, sw_constructor)
}

// ── Constructor type aliases (enable unit-test injection) ─────────────────────

type HwResult = Result<Box<dyn VideoEncoder + Send + Sync>, EncoderError>;
type SwResult = Result<Box<dyn VideoEncoder + Send + Sync>, EncoderError>;

/// Seam for unit tests: a function that constructs the HW encoder (or a mock).
type HwEncoderConstructor = fn(EncoderConfig) -> HwResult;
/// Seam for unit tests: a function that constructs the SW encoder (or a mock).
type SwEncoderConstructor = fn(EncoderConfig) -> SwResult;

fn hw_constructor(config: EncoderConfig) -> HwResult {
    #[cfg(feature = "hw-encoder")]
    {
        WindowsMftH264Encoder::new(config)
            .map(|enc| Box::new(enc) as Box<dyn VideoEncoder + Send + Sync>)
    }
    #[cfg(not(feature = "hw-encoder"))]
    {
        let _ = config;
        Err(EncoderError::InitFailed(
            "hw-encoder feature is disabled".into(),
        ))
    }
}

fn sw_constructor(config: EncoderConfig) -> SwResult {
    WindowsOpenH264Encoder::new(config)
        .map(|enc| Box::new(enc) as Box<dyn VideoEncoder + Send + Sync>)
}

// ── Core decision logic (testable via injected constructors) ──────────────────

fn build_video_encoder_with(
    config: EncoderConfig,
    hw: HwEncoderConstructor,
    sw: SwEncoderConstructor,
) -> Result<Box<dyn VideoEncoder + Send + Sync>, EncoderError> {
    // Step 1: env-var override.
    let force_sw = std::env::var("SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER").as_deref() == Ok("1");

    if !force_sw {
        // Step 2: attempt HW encoder.
        match hw(config.clone()) {
            Ok(enc) => return Ok(enc),
            Err(EncoderError::InitFailed(reason)) => {
                log_sw_fallback_once(&reason);
                // Fall through to SW.
            }
            Err(other) => return Err(other),
        }
    }

    // Step 3: SW fallback.
    sw(config)
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use sm_domain::encode::{EncoderConfig, EncoderError};

    // Helper mock: HW constructor that always returns InvalidConfig.
    fn hw_invalid_config(_: EncoderConfig) -> HwResult {
        Err(EncoderError::InvalidConfig(
            "bitrate_bps must be > 0".into(),
        ))
    }

    // Helper mock: HW constructor that always returns InitFailed (no hardware).
    fn hw_init_failed(_: EncoderConfig) -> HwResult {
        Err(EncoderError::InitFailed(
            "no hardware MFT encoder found".into(),
        ))
    }

    // Helper mock: SW constructor that always succeeds (returns a FakeVideoEncoder-like box).
    // We use WindowsOpenH264Encoder with default config as a real SW impl for the "ok" path.
    // Since this is a unit test with a valid config, SW should succeed.
    fn sw_succeeds(config: EncoderConfig) -> SwResult {
        WindowsOpenH264Encoder::new(config)
            .map(|enc| Box::new(enc) as Box<dyn VideoEncoder + Send + Sync>)
    }

    // ─── T5.2.1: env_var_override_selects_software_encoder ────────────────────
    // Set SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER=1 → SW path taken, HW never tried.
    // We verify by passing an HW constructor that would return InvalidConfig
    // (which would propagate without fallback if HW was tried and returned InvalidConfig).
    // But since env-var bypasses HW entirely, SW runs and succeeds.
    #[test]
    fn env_var_override_selects_software_encoder() {
        // SAFETY: nextest runs each test in its own process (isolates env mutations).
        // set_var/remove_var are unsafe on Rust 1.81+ due to POSIX thread-safety requirements.
        unsafe { std::env::set_var("SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER", "1") };

        let config = EncoderConfig::default(); // valid config
        let result = build_video_encoder_with(config, hw_invalid_config, sw_succeeds);

        unsafe { std::env::remove_var("SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER") };

        // SW path was taken; HW (which would have returned InvalidConfig) was skipped.
        assert!(
            result.is_ok(),
            "expected SW encoder to succeed when force_sw=1, got Err"
        );
    }

    // ─── T5.2.2: init_failed_falls_back_to_software_encoder ──────────────────
    // HW returns InitFailed → factory falls back to SW → returns Ok(box).
    #[test]
    fn init_failed_falls_back_to_software_encoder() {
        // SAFETY: nextest process isolation prevents env var cross-test pollution.
        unsafe { std::env::remove_var("SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER") };

        let config = EncoderConfig::default();
        let result = build_video_encoder_with(config, hw_init_failed, sw_succeeds);

        assert!(
            result.is_ok(),
            "expected SW fallback after HW InitFailed, got Err"
        );
    }

    // ─── T5.2.3: invalid_config_propagates_without_fallback ──────────────────
    // HW returns InvalidConfig → factory propagates it immediately (no SW try).
    #[test]
    fn invalid_config_propagates_without_fallback() {
        // SAFETY: nextest process isolation prevents env var cross-test pollution.
        unsafe { std::env::remove_var("SCREEN_MIRROR_FORCE_SOFTWARE_ENCODER") };

        let config = EncoderConfig::default();
        let result = build_video_encoder_with(config, hw_invalid_config, sw_succeeds);

        match result {
            Err(EncoderError::InvalidConfig(_)) => {}
            Err(other) => {
                panic!("expected InvalidConfig to propagate without SW fallback, got {other:?}")
            }
            Ok(_) => panic!("expected InvalidConfig to propagate but factory returned Ok"),
        }
    }
}
