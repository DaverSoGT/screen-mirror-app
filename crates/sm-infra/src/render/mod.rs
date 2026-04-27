// Render-side helpers (see decoder-h264-windows change)
//
// V1: `fmp4_muxer` (ISO 14496-12 fragmented MP4 builder for the MSE hot path)
//      + `avcc` (SPS parser + AVCDecoderConfigurationRecord builder).

pub mod avcc;
pub mod fmp4_muxer;
