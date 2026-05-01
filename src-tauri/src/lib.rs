pub mod commands;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(commands::stream::StreamBridge::new())
        .manage(commands::sender::SenderBridge::new())
        .invoke_handler(tauri::generate_handler![
            commands::stream::start_stream,
            commands::stream::stop_stream,
            commands::stream::attach_stream,
            commands::stream::stream_diagnostics,
            commands::sender::start_sender,
            commands::sender::stop_sender,
            commands::sender::sender_diagnostics,
            commands::sender::retry_session,
            smoke::smoke_com_apartment,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

mod smoke {
    //! R10.2 — COM apartment compatibility smoke test for Tauri 2.
    //!
    //! Invoked from the frontend (see `dist/index.html`) after the WebView is
    //! ready. Calls `WindowsCaptureSource::start()` from a worker thread spawned
    //! by Tauri's main runtime and asserts >= 3 frames are received within 10s
    //! without panic, deadlock, or COM apartment error.
    //!
    //! Returns `Ok(message)` on success; the frontend renders the message to
    //! confirm R10.2 PASS. On non-Windows builds the command is a no-op stub.

    #[cfg(target_os = "windows")]
    #[tauri::command]
    pub fn smoke_com_apartment() -> Result<String, String> {
        use std::sync::mpsc;
        use std::thread;
        use std::time::{Duration, Instant};

        use sm_domain::{CaptureConfig, CaptureSource};
        use sm_infra::capture::{CAPTURE_CHANNEL_CAPACITY, WindowsCaptureSource};

        let join = thread::spawn(|| -> Result<u32, String> {
            let (tx, rx) = mpsc::sync_channel(CAPTURE_CHANNEL_CAPACITY);
            let mut src = WindowsCaptureSource::new(CaptureConfig::default())
                .map_err(|e| format!("new() failed: {e}"))?;
            src.start(tx).map_err(|e| format!("start() failed: {e}"))?;

            let deadline = Instant::now() + Duration::from_secs(10);
            let mut frames = 0u32;
            while Instant::now() < deadline && frames < 3 {
                if rx.recv_timeout(Duration::from_millis(500)).is_ok() {
                    frames += 1;
                }
            }

            src.stop().map_err(|e| format!("stop() failed: {e}"))?;

            if frames < 3 {
                return Err(format!("only {frames} frames in 10s (expected >= 3)"));
            }
            Ok(frames)
        });

        match join.join() {
            Ok(Ok(frames)) => Ok(format!(
                "R10.2 PASS — {frames} frames captured under Tauri 2 runtime, no apartment conflict"
            )),
            Ok(Err(e)) => Err(format!("R10.2 FAIL — {e}")),
            Err(_) => Err("R10.2 FAIL — capture worker thread panicked".to_string()),
        }
    }

    #[cfg(not(target_os = "windows"))]
    #[tauri::command]
    pub fn smoke_com_apartment() -> Result<String, String> {
        Err("R10.2 smoke is Windows-only".to_string())
    }
}
