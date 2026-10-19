import { overwriteGetLocale, type Locale } from './paraglide/runtime';

/** «Русский», «English» or «Как в системе» — the last takes the language of
 *  the system's interface: Russian for a Russian one, English for any other. */
export type LangMode = Locale | 'system';

/** The window's own choice, kept next to the theme in the WebView's storage.
 *  The service is told the resolved language separately (see `onLocale`). */
const KEY = 'ownbackup.lang';

/** WebView2 reports the language of the Windows interface (it is not told
 *  otherwise), WebKitGTK the session's locale. The boot medium is built on an
 *  en-US recovery environment, so «Как в системе» is English there. */
function systemLocale(): Locale {
  const tags = navigator.languages?.length ? navigator.languages : [navigator.language];
  return tags.some((t) => t?.toLowerCase().startsWith('ru')) ? 'ru' : 'en';
}

function loadMode(): LangMode {
  try {
    const v = localStorage.getItem(KEY);
    if (v === 'ru' || v === 'en' || v === 'system') return v;
  } catch {
    // Storage refused: the default.
  }
  return 'system';
}

export const lang = $state({ mode: loadMode() });

let current = $state<Locale>(resolve(lang.mode));

function resolve(mode: LangMode): Locale {
  return mode === 'system' ? systemLocale() : mode;
}

// Every message function asks getLocale(); answering from a $state makes each
// text on screen depend on it, so switching re-renders in place — no reload,
// nothing half-typed in a wizard is lost. Paraglide's own setLocale (which
// reloads the document) is never called.
overwriteGetLocale(() => current);

/** The language the window speaks right now. */
export function locale(): Locale {
  return current;
}

/** BCP 47 tag for Intl formatters and toLocaleString. */
export function intlLocale(): string {
  return current === 'ru' ? 'ru-RU' : 'en-US';
}

/** For the few sentences that carry markup inside (`{@html m.x({ v: esc(v) })}`):
 *  the message owns its <b>/<code> tags, anything interpolated is escaped. */
export function esc(s: unknown): string {
  return String(s ?? '')
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}

const listeners: ((l: Locale) => void)[] = [];

/** Called with the resolved language now and on every change — for whatever
 *  lives outside this window and speaks too (the service's messages). */
export function onLocale(fn: (l: Locale) => void) {
  listeners.push(fn);
  fn(current);
}

function apply() {
  current = resolve(lang.mode);
  document.documentElement.lang = current;
  for (const fn of listeners) fn(current);
}

export function setLanguage(mode: LangMode) {
  if (mode === lang.mode) return;
  lang.mode = mode;
  try {
    localStorage.setItem(KEY, mode);
  } catch {
    // Not remembered, still applied.
  }
  apply();
}

let started = false;

/** Called once, before the app is mounted. */
export function initI18n() {
  if (started) return;
  started = true;
  // The Windows interface language changes only with a sign-out, but the
  // WebView says so when it does; «Как в системе» follows.
  window.addEventListener('languagechange', () => {
    if (lang.mode === 'system') apply();
  });
  document.documentElement.lang = current;
}
