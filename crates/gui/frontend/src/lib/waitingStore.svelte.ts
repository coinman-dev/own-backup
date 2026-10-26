// What waits its turn in the service: an operation a person started on a
// chain's folder, the medium builder or a stick, while something else holds
// it. The service queues it instead of refusing (`Busy::reserve_queued`) and
// says so with `waiting` events; the window shows one line per such
// operation, with the stop button that reaches it while it waits.

import { listen } from '@tauri-apps/api/event';

export interface WaitingOp {
  id: string;
  /** What waits, as the service names it: `what (which)`. */
  what: string;
  /** What it waits behind. */
  behind: string;
}

interface WaitingEvent {
  id: string;
  waiting: boolean;
  what?: string;
  behind?: string;
}

export const waiting = $state({ ops: [] as WaitingOp[] });

let started = false;

export function initWaiting(): void {
  if (started) return;
  started = true;
  void listen<WaitingEvent>('busy-waiting', (e) => {
    const p = e.payload;
    const rest = waiting.ops.filter((o) => o.id !== p.id);
    waiting.ops = p.waiting
      ? [...rest, { id: p.id, what: p.what ?? '', behind: p.behind ?? '' }]
      : rest;
  });
}
