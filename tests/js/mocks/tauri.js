import { vi } from 'vitest';

// Minimal Channel impl matching the Tauri 2 Channel<T> surface:
//   new Channel() instance has .onmessage setter
//   tests push payloads via the test-only ._dispatch(payload) helper
export class MockChannel {
  constructor() {
    this.onmessage = null;
    this.id = MockChannel._nextId++;
    MockChannel._registry.set(this.id, this);
  }
  _dispatch(payload) {
    if (typeof this.onmessage === 'function') this.onmessage(payload);
  }
  static _registry = new Map();
  static _nextId = 1;
  static reset() {
    MockChannel._registry.clear();
    MockChannel._nextId = 1;
  }
}

// installTauriMock() returns a handle so tests can inspect/script invoke calls.
export function installTauriMock() {
  const invoke = vi.fn().mockResolvedValue(undefined);
  MockChannel.reset();
  globalThis.__TAURI__ = {
    core: {
      invoke,
      Channel: MockChannel,
    },
  };
  return {
    invoke,
    Channel: MockChannel,
    // Find the most recently constructed Channel so tests can push frames:
    lastChannel: () => {
      const ids = [...MockChannel._registry.keys()];
      return MockChannel._registry.get(ids[ids.length - 1]) ?? null;
    },
  };
}

export function resetTauriMock() {
  MockChannel.reset();
  if (globalThis.__TAURI__?.core?.invoke) {
    globalThis.__TAURI__.core.invoke.mockReset();
    globalThis.__TAURI__.core.invoke.mockResolvedValue(undefined);
  }
}
