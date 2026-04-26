//! Border policy visual smoke test for Windows 11 22H2+.
//!
//! Captures the primary monitor for ~10 seconds with a chosen `BorderPolicy`,
//! discarding frames. The operator visually confirms whether the WGC capture
//! border (yellow rectangle around the captured monitor) appears.
//!
//! # Acceptance criteria
//!
//! - On Windows 11 build >= 22621 with `BorderPolicy::Auto`     → no border visible.
//! - On Windows 11 build >= 22621 with `BorderPolicy::AlwaysOn` → border visible.
//! - On Windows 11 build  < 22621 with `BorderPolicy::Auto`     → border visible
//!   (older WGC has no opt-out, `Auto` falls back to `AlwaysOn`).
//!
//! # Usage
//!
//! ```text
//! cargo run -p sm-infra --example border_smoke -- auto
//! cargo run -p sm-infra --example border_smoke -- on
//! cargo run -p sm-infra --example border_smoke -- off
//! ```

#[cfg(target_os = "windows")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use sm_domain::{BorderPolicy, CaptureConfig, CaptureSource};
    use sm_infra::capture::{CAPTURE_CHANNEL_CAPACITY, WindowsCaptureSource};

    let arg = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "auto".to_string());
    let border = match arg.as_str() {
        "auto" => BorderPolicy::Auto,
        "on" => BorderPolicy::AlwaysOn,
        "off" => BorderPolicy::AlwaysOff,
        other => {
            eprintln!("usage: border_smoke <auto|on|off> (got '{other}')");
            std::process::exit(2);
        }
    };

    let cfg = CaptureConfig {
        border,
        ..Default::default()
    };
    let (tx, rx) = mpsc::sync_channel(CAPTURE_CHANNEL_CAPACITY);
    let mut src = WindowsCaptureSource::new(cfg)?;
    src.start(tx)?;

    println!("BorderPolicy::{border:?} — observa el monitor primario durante 10 segundos");
    println!("(press Ctrl+C para abortar)");

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut frames = 0u32;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(_) => frames += 1,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    src.stop()?;
    thread::sleep(Duration::from_millis(50));

    println!(
        "captura terminada — {frames} frames en 10s, dropped_frames={}",
        src.dropped_frames()
    );
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("border_smoke is Windows-only");
    std::process::exit(1);
}
