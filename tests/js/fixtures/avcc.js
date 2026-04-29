// avcC payload layout (relative to the 'avcC' tag at offset i):
//   i+0  'a'                      0x61
//   i+1  'v'                      0x76
//   i+2  'c'                      0x63
//   i+3  'C'                      0x43
//   i+4  configurationVersion     0x01
//   i+5  AVCProfileIndication     (profile_idc)
//   i+6  profile_compatibility    (constraint_set_flags)
//   i+7  AVCLevelIndication       (level_idc)
//   i+8  ...
//
// deriveCodecFromInitSegment scans the buffer for the 'avcC' four-CC,
// then reads bytes at i+5, i+6, i+7 to produce the codec string.

// High@4.1 (1080p) — profile 0x64, compat 0x00, level 0x29
// Expected codec: 'video/mp4; codecs="avc1.640029"'
export const AVCC_HIGH_41 = new Uint8Array([
  0x61, 0x76, 0x63, 0x43,  // 'avcC'
  0x01,                    // configurationVersion
  0x64,                    // AVCProfileIndication = High (100 = 0x64)
  0x00,                    // profile_compatibility = 0
  0x29,                    // AVCLevelIndication = 4.1 (41 = 0x29)
  0xff,                    // (filler — lengthSizeMinusOne etc.)
]);

// Baseline@3.0 — profile 0x42, compat 0xE0, level 0x1E
// Expected codec: 'video/mp4; codecs="avc1.42E01E"'
export const AVCC_BASELINE_30 = new Uint8Array([
  0x61, 0x76, 0x63, 0x43,  // 'avcC'
  0x01,                    // configurationVersion
  0x42,                    // AVCProfileIndication = Baseline (66 = 0x42)
  0xE0,                    // profile_compatibility (constraint set bits 0-5)
  0x1E,                    // AVCLevelIndication = 3.0 (30 = 0x1E)
  0xff,
]);

// Main@4.0 — profile 0x4D, compat 0x40, level 0x28
// Expected codec: 'video/mp4; codecs="avc1.4D4028"'
export const AVCC_MAIN_40 = new Uint8Array([
  0x61, 0x76, 0x63, 0x43,  // 'avcC'
  0x01,                    // configurationVersion
  0x4D,                    // AVCProfileIndication = Main (77 = 0x4D)
  0x40,                    // profile_compatibility
  0x28,                    // AVCLevelIndication = 4.0 (40 = 0x28)
  0xff,
]);

// Buffer with NO avcC anywhere — must yield null
export const NO_AVCC = new Uint8Array([
  0x66, 0x74, 0x79, 0x70,  // 'ftyp'
  0x69, 0x73, 0x6f, 0x6d,  // 'isom'
  0x00, 0x00, 0x00, 0x00,
]);
