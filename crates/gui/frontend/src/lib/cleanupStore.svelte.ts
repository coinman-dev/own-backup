// «Очистка»'s analysis and its marks, kept while the window is open: going to
// another section and back must not lose them (owner, 2026-10-28). Nothing
// here survives closing the window — the service's own list of an analysis
// does not either (it is in memory, and the next analysis replaces it).
import type { CleanupReport, CleanupTally } from './types';

export const cleanupState = $state({
  /** Per item: what the last analysis found, or what the last cleanup removed. */
  tallies: {} as Record<string, CleanupTally & { running?: string[] }>,
  /** What the numbers on screen are. */
  shown: 'none' as 'none' | 'analyze' | 'run',
  /** An analysis or a cleanup this window started is under way. */
  busy: null as 'analyze' | 'run' | null,
  report: null as CleanupReport | null,
  /** The users or options changed after an analysis. */
  stale: false,
  /** Per item: the files of the analysis' list unticked in its window. */
  excluded: {} as Record<string, Map<string, number>>,
  /** Items whose list was opened since the analysis. */
  reviewed: new Set<string>(),
});
