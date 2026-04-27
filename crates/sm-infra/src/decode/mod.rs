// Decode adapters (see decoder-h264-windows change)
//
// V1: `windows_openh264::WindowsOpenH264Decoder` (capability tier — NOT on hot path
// per PQ-D6; production path is the fMP4/MSE bridge in `sm_infra::render`).
// V2: a future `windows_mf::WindowsMfDecoder` behind the `hw-decoder` Cargo feature.
//
// Module stubs are empty until B1/B2/B3 fill them in.

pub mod i420_to_bgra;

#[cfg(target_os = "windows")]
pub mod windows_openh264;
