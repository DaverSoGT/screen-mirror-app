// Minimal media-segment fixture with leading discriminant byte (FRAME_SEGMENT=0x01)
// for the Channel push path. The mp4 box parser is mocked away; tests only
// assert that the byte AT offset 0 of what is appended is NOT 0x01 (B11-S7).
export const FRAME_SEGMENT_DISCRIMINANT = 0x01;
export const FRAME_INIT_DISCRIMINANT = 0x00;

// 32-byte fake moof+mdat payload (not parsed; just verifies pass-through).
// byteLength = 32 so B11-S7 tests can assert sb.appendBuffer receives 32 bytes.
export const FAKE_PAYLOAD = new Uint8Array([
  0x00, 0x00, 0x00, 0x10, 0x6d, 0x6f, 0x6f, 0x66,  // box size (16) + 'moof'
  0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,   // filler
  0x00, 0x00, 0x00, 0x10, 0x6d, 0x64, 0x61, 0x74,   // box size (16) + 'mdat'
  0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,   // payload bytes
]);

// Prepend discriminant 0x01 (FRAME_SEGMENT) to the fake payload.
// Total length = 1 + 32 = 33 bytes.
// After mse-client.js strips the discriminant via data.subarray(1),
// appendBuffer should receive 32 bytes.
export function makeMediaSegmentFrame() {
  const out = new Uint8Array(1 + FAKE_PAYLOAD.length);
  out[0] = FRAME_SEGMENT_DISCRIMINANT;
  out.set(FAKE_PAYLOAD, 1);
  return out;
}

// Prepend discriminant 0x00 (FRAME_INIT) to an init-segment fixture.
export function makeInitFrame(initBytes) {
  const out = new Uint8Array(1 + initBytes.length);
  out[0] = FRAME_INIT_DISCRIMINANT;
  out.set(initBytes, 1);
  return out;
}
