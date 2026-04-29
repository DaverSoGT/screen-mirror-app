import { AVCC_HIGH_41, AVCC_BASELINE_30, AVCC_MAIN_40, NO_AVCC } from './avcc.js';

// Construct an init-segment-shaped buffer with ftyp prefix + filler + avcC.
// The exact box structure is NOT validated by mse-client.js — it only scans
// linearly for the 'avcC' tag. This is sufficient.
function withFtypPrefix(avcc) {
  const ftyp = new Uint8Array([
    0x00, 0x00, 0x00, 0x18, // box size = 24 bytes
    0x66, 0x74, 0x79, 0x70, // 'ftyp'
    0x69, 0x73, 0x6f, 0x36, // major brand 'iso6'
    0x00, 0x00, 0x00, 0x01, // minor version
    0x69, 0x73, 0x6f, 0x6d, // compat brand 'isom'
    0x69, 0x73, 0x6f, 0x36, // compat brand 'iso6'
  ]);
  const out = new Uint8Array(ftyp.length + avcc.length);
  out.set(ftyp, 0);
  out.set(avcc, ftyp.length);
  return out;
}

// Full init segment with ftyp prefix + avcC box (High@4.1)
// Used for FRAME_INIT path tests (discriminant byte prepended separately)
export const INIT_HIGH_41 = withFtypPrefix(AVCC_HIGH_41);

// Full init segment with ftyp prefix + avcC box (Baseline@3.0)
export const INIT_BASELINE_30 = withFtypPrefix(AVCC_BASELINE_30);

// Full init segment with ftyp prefix + avcC box (Main@4.0)
export const INIT_MAIN_40 = withFtypPrefix(AVCC_MAIN_40);

// Full init segment WITHOUT avcC (only ftyp + filler that has no 'avcC' bytes)
export const INIT_MISSING_AVCC = withFtypPrefix(NO_AVCC);
