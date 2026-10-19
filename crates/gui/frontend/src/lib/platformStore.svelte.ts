// Which system the window runs on. One answer, asked once, read from anywhere.
//
// Parts of the interface exist on one system only: drive letters for hidden
// partitions and the WebView2 runtime are Windows things, the Unix socket and
// /var/log are Linux ones, and a path is joined with the separator its system
// uses. The backend decides (`platform_info`); this caches the answer, which
// cannot change while the program runs.

import { api } from './api';

/** A first answer before the backend's arrives, so the first draw is right:
 *  the webview names its system in the user agent (WebView2 says Windows,
 *  WebKitGTK says Linux/X11). Replaced by `platform_info` as soon as it comes. */
function guessOs(): string {
  return /windows/i.test(navigator.userAgent) ? 'windows' : 'linux';
}

export const platform = $state({
  /** `windows` or `linux`. */
  os: guessOs(),
  /** The service channel: `\\.\pipe\OwnBackup` or `/run/own-backup/service.sock`. */
  channel: '',
  /** Where this window logs, when it could open a folder for it. */
  uiLogDir: null as string | null,
  /** Where the service logs, where that is a fixed folder (Linux). */
  serviceLogDir: null as string | null,
  /** False until the backend has answered. */
  known: false,
});

/** Windows: drive letters, the named pipe, `\` in paths. */
export function isWindows(): boolean {
  return platform.os === 'windows';
}

let asked: Promise<void> | null = null;

/** Ask once; later callers share the same answer. */
export function ensurePlatform(): Promise<void> {
  asked ??= api.platformInfo().then(
    (p) => {
      platform.os = p.os;
      platform.channel = p.channel;
      platform.uiLogDir = p.ui_log_dir;
      platform.serviceLogDir = p.service_log_dir;
      platform.known = true;
    },
    () => {
      // An older backend without the command: the guess stands.
    },
  );
  return asked;
}
