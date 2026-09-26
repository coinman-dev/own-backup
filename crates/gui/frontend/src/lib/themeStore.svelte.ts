import { getCurrentWindow } from '@tauri-apps/api/window';

/** «Светлая», «Тёмная» or «Как в Windows» — the last follows the system's
 *  «Режим приложения» and switches along with it. */
export type ThemeMode = 'light' | 'dark' | 'system';

/** A choice of this window, not of the machine: kept in the WebView's own
 *  storage, never sent to the service. On the boot medium that storage lives
 *  in RAM and the choice is gone after a reboot — as is everything else. */
const KEY = 'ownbackup.theme';

const darkQuery = window.matchMedia('(prefers-color-scheme: dark)');

function loadMode(): ThemeMode {
  try {
    const v = localStorage.getItem(KEY);
    if (v === 'light' || v === 'dark' || v === 'system') return v;
  } catch {
    // Storage refused (it can be, in a locked-down WebView): the default.
  }
  return 'system';
}

export const theme = $state({
  mode: loadMode(),
  /** What Windows says right now; read only in «Как в Windows». */
  systemDark: darkQuery.matches,
});

export function effectiveTheme(): 'light' | 'dark' {
  if (theme.mode === 'system') return theme.systemDark ? 'dark' : 'light';
  return theme.mode;
}

function apply() {
  document.documentElement.dataset.theme = effectiveTheme();
}

/** The title bar is Windows', not ours: it follows the system unless told
 *  otherwise. Telling it also tells the WebView — `prefers-color-scheme` then
 *  answers what was set, not what the system says — so «Как в Windows» hands
 *  both back (`null`) and reads the real answer again once that is done. */
async function applyWindow() {
  const mode = theme.mode;
  try {
    await getCurrentWindow().setTheme(mode === 'system' ? null : mode);
  } catch {
    // Older exe without the permission, or not inside Tauri: the page itself
    // is themed all the same, only the title bar keeps the system's colours.
  }
  if (mode === 'system' && theme.mode === 'system') {
    theme.systemDark = darkQuery.matches;
    apply();
  }
}

export function setTheme(mode: ThemeMode) {
  if (mode === theme.mode) return;
  theme.mode = mode;
  try {
    localStorage.setItem(KEY, mode);
  } catch {
    // Not remembered, still applied.
  }
  apply();
  applyWindow();
}

let started = false;

/** Called once, before the app is mounted, so the first frame is already in
 *  the right colours. */
export function initTheme() {
  if (started) return;
  started = true;
  darkQuery.addEventListener('change', (e) => {
    theme.systemDark = e.matches;
    if (theme.mode === 'system') apply();
  });
  apply();
  if (theme.mode !== 'system') applyWindow();
}
