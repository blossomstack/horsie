/**
 * jsdom implements no layout, so it ships no `ResizeObserver`. A component
 * that measures itself is a real component, not a test artifact — stub the API
 * here rather than making production code carry a `typeof` guard that exists
 * only for the test environment.
 *
 * The stub never fires: a jsdom element never changes size. Tests that care
 * about a measurement stage `scrollHeight` directly.
 */
class NoopResizeObserver implements ResizeObserver {
  observe(): void {}
  unobserve(): void {}
  disconnect(): void {}
}

globalThis.ResizeObserver ??= NoopResizeObserver;
