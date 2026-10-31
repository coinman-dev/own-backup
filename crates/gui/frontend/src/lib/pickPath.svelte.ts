// "Choose a folder / choose a file", asked from anywhere, answered one of two
// ways.
//
// On an installed Windows the native shell dialog is the right answer: it is
// what the user knows, with their pinned places and network shares. On the boot
// medium there is no shell for it to come from — the call returns "cancelled"
// and the button looks broken (found on the medium 2026-07-31: the destination
// could not be chosen at all). There we open our own browser instead, built on
// the list_roots/list_dir commands.
//
// Callers do not choose: they await pickPath() and get a path or null.

import { open } from '@tauri-apps/plugin-dialog';
import { media, ensureMediaMode } from './mediaStore.svelte';

export interface PickRequest {
  /** Pick a directory (true) or a file (false). */
  directory: boolean;
  title: string;
  /** File extensions to show, lower-case and without the dot. Files only. */
  extensions?: string[];
  /** Where to start; the last visited directory is used when omitted. */
  startPath?: string;
  /** The chosen folder will be written to (the Linux medium mounts read-only). */
  writable?: boolean;
}

/** State the modal renders from. `open` is false when nothing is being picked. */
export const picker = $state({
  open: false,
  request: null as PickRequest | null,
});

let resolveCurrent: ((path: string | null) => void) | null = null;

/** Called by the modal when the user chooses or cancels. */
export function finishPick(path: string | null) {
  picker.open = false;
  picker.request = null;
  const r = resolveCurrent;
  resolveCurrent = null;
  r?.(path);
}

export async function pickPath(req: PickRequest): Promise<string | null> {
  await ensureMediaMode();
  if (!media.active) {
    const picked = await open({
      directory: req.directory,
      multiple: false,
      title: req.title,
      filters: req.extensions?.length
        ? [{ name: req.title, extensions: req.extensions }]
        : undefined,
    });
    return typeof picked === 'string' ? picked : null;
  }
  // Only one picker at a time: a second request cancels the first rather than
  // leaving its caller waiting for a promise nothing will resolve.
  finishPick(null);
  picker.request = req;
  picker.open = true;
  return new Promise<string | null>((resolve) => {
    resolveCurrent = resolve;
  });
}
