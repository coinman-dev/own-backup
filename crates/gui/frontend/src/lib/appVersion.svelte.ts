import { getVersion } from '@tauri-apps/api/app';

/** The window's own version, from the build (the workspace's Cargo version),
 *  asked once. It was typed into the menu as «v0.1.0» and stayed there while
 *  the build moved on to 0.1.1. Empty until the answer comes. */
export const appVersion = $state({ value: '' });

let asked = false;
export function ensureAppVersion(): void {
  if (asked) return;
  asked = true;
  getVersion().then(
    (v) => (appVersion.value = v),
    () => {},
  );
}
