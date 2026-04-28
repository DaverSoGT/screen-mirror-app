//! Tauri command modules for screen-mirror.
//!
//! Commands registered here are exposed to the WebView frontend via
//! `tauri::generate_handler!` in `src-tauri/src/lib.rs`.

pub mod sender;
pub mod stream;
