// A live reading shown at a pace a person can read: the figures under a
// progress bar changed with every message from the service — several times a
// second — and the speed and the time left jumped about so that neither could
// be read (2026-11-19). Now they change once a second.

/** How a reading is condensed into what is shown each second. */
export type SteadyMode = 'mean' | 'latest';

/**
 * Shown value of `read()`, refreshed once a second: the mean of what arrived
 * in that second (`mean`, for speeds and times) or the newest (`latest`, for
 * counters). The first value is shown at once; `NaN` (nothing to show — a new
 * phase, no estimate yet) is passed through at once and starts afresh.
 *
 * Call during component setup: it uses `$effect`.
 */
export function steady(read: () => number, mode: SteadyMode = 'mean', everyMs = 1000) {
  const shown = $state({ value: NaN });
  let sum = 0;
  let count = 0;
  let latest = NaN;

  $effect(() => {
    const v = read();
    if (!Number.isFinite(v)) {
      sum = 0;
      count = 0;
      latest = NaN;
      shown.value = NaN;
      return;
    }
    sum += v;
    count += 1;
    latest = v;
    if (!Number.isFinite(shown.value)) shown.value = v;
  });

  $effect(() => {
    const id = setInterval(() => {
      if (count === 0) return;
      shown.value = mode === 'mean' ? sum / count : latest;
      sum = 0;
      count = 0;
    }, everyMs);
    return () => clearInterval(id);
  });

  return shown;
}
