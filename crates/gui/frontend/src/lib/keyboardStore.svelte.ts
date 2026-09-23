// What the keyboard is doing, for the password fields: which layout the
// keystrokes go through, and whether Caps Lock is on. A password typed in the
// wrong layout, or in capitals, fails exactly like a wrong password — and with
// the characters masked, nothing on screen says which of the three it was.
//
// Two sources, each covering the other's blind spot. Nothing inside a WebView
// can say which language the keyboard is in before a key arrives, so the layout
// is asked of Windows (`keyboard_state`). Caps Lock is the other way round:
// every key and mouse event carries the browser's own answer, the one the
// typing actually goes through — so events win, and the polled value is taken
// only when it CHANGES (the lamp was toggled while another window had the
// keyboard), or while no event has said anything yet.

import { api } from './api';
import type { KeyboardLayout } from './types';

export const kbd = $state({
  layout: null as KeyboardLayout | null,
  /** null until something has said. */
  caps: null as boolean | null,
});

/** How often Windows is asked while a password field is on screen. */
const POLL_MS = 400;

let fields = 0;
let timer: ReturnType<typeof setInterval> | null = null;
let inflight = false;
/** The last polled Caps Lock; `undefined` before the first poll. */
let polledCaps: boolean | null | undefined;

async function poll(): Promise<void> {
  if (inflight) return;
  inflight = true;
  try {
    const s = await api.keyboardState();
    const l = s.layout;
    // A fresh object every tick would redraw every field every tick.
    if (l?.code !== kbd.layout?.code || l?.name !== kbd.layout?.name) kbd.layout = l;
    const c = s.caps_lock;
    if (c !== null) {
      const changed = polledCaps === undefined ? kbd.caps === null : c !== polledCaps;
      if (changed && kbd.caps !== c) kbd.caps = c;
      polledCaps = c;
    }
  } catch {
    // Older exe without the command: the field shows what events tell it.
  } finally {
    inflight = false;
  }
}

function fromEvent(e: KeyboardEvent | MouseEvent): void {
  if (typeof e.getModifierState !== 'function') return;
  const on = e.getModifierState('CapsLock');
  if (kbd.caps !== on) kbd.caps = on;
}

// Installed once, for the life of the window, and in the capture phase: the
// click that opens a password dialog happens before the dialog's field exists,
// and it is the freshest word on Caps Lock there is.
if (typeof window !== 'undefined') {
  const opts = { capture: true, passive: true };
  window.addEventListener('keydown', fromEvent, opts);
  window.addEventListener('mousedown', fromEvent, opts);
  window.addEventListener('mousemove', fromEvent, opts);
  window.addEventListener(
    'keyup',
    (e) => {
      fromEvent(e);
      // Alt+Shift, Ctrl+Shift, Win+Space: the layout may just have changed.
      // Asked now rather than on the next tick, so the badge keeps up.
      if (fields > 0 && ['Shift', 'Alt', 'Control', 'Meta'].includes(e.key)) poll();
    },
    opts,
  );
  window.addEventListener('focus', () => {
    if (fields > 0) poll();
  });
}

/** A password field is on screen: keep the state current until the returned
 *  function is called. Any number of fields share one poll. */
export function watchKeyboard(): () => void {
  fields++;
  if (fields === 1) {
    poll();
    timer = setInterval(() => {
      if (!document.hidden) poll();
    }, POLL_MS);
  }
  return () => {
    fields--;
    if (fields === 0 && timer !== null) {
      clearInterval(timer);
      timer = null;
    }
  };
}
