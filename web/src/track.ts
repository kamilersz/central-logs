/**
 * Explicit UI-action tracking for the dashboard's own dogfood analytics.
 *
 * Pageviews only capture URL changes, so filter interactions, dropdown
 * selections and button actions (which are pure React state) would otherwise
 * be invisible in the `central-logs` web-analytics project. Call
 * `trackEvent("ui.<page>.<action>", props)` from discrete handlers or
 * settled (debounced) filter states.
 *
 * Safe no-op when the tracker isn't loaded (collector disabled, DNT/GPC,
 * ad-blocker ate t.js): `window.cl` is only defined by the tracker script.
 * Props are clipped server-side (256 chars); keep them low-cardinality
 * except for filter/search text, which is the point.
 */

export type TrackProps = Record<string, string | number | boolean>;

export function trackEvent(name: string, props?: TrackProps): void {
  try {
    const w = window as unknown as {
      cl?: { track?: (n: string, p?: TrackProps) => void };
    };
    w.cl?.track?.(name, props);
  } catch {
    /* analytics must never break the UI */
  }
}
