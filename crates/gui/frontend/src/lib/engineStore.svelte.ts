// Shared, app-lifetime state for volume enumeration and the Restore/Tools
// engine ops (restore/verify/mount). Backups run through jobStore's queue.

import { listen } from '@tauri-apps/api/event';
import { api } from './api';
import { formatBytes } from './format';
import { m } from './paraglide/messages';
import { onLocale } from './i18n.svelte';
import type {
  ArchiveSecret,
  DiskInfo,
  EngineProgress,
  RestoreSummary,
  VerifySummary,
} from './types';

export interface VolumeOption {
  device: string;
  label: string;
}

export const engine = $state({
  // --- volume cache (shared by wizard source + restore target) ---
  volumes: [] as VolumeOption[],
  // Raw physical disks with their partitions — the Paragon-style source map in
  // the wizard renders from this; `volumes` is the flattened letter list.
  disks: [] as DiskInfo[],
  volumesLoaded: false,
  volumesLoading: false,
  volumesError: null as string | null,
  driveSignature: -1,

  // --- restore / verify op ---
  op: null as 'restore' | 'verify' | null,
  restoreProgress: null as EngineProgress | null,
  restoreSummary: null as RestoreSummary | null,
  restoreError: null as string | null,
  verifyProgress: null as EngineProgress | null,
});

/** When the running restore started and how fast it has been going lately.
 *  In the store, like the progress itself: the restore view is unmounted the
 *  moment the user walks through the menu, and a clock kept there started
 *  again from zero on the way back (2026-09-18). */
export const restoreClock = $state({
  startMs: 0,
  samples: [] as { t: number; blocks: number }[],
});

/** How long the restore still has at the rate of the last few seconds, or
 *  `NaN` while the window is too short to say anything. */
export function restoreEtaMs(): number {
  const p = engine.restoreProgress;
  const s = restoreClock.samples;
  if (!p || s.length < 2) return NaN;
  const dt = s[s.length - 1].t - s[0].t;
  const db = s[s.length - 1].blocks - s[0].blocks;
  if (dt < 1500 || db <= 0) return NaN;
  return ((p.blocks_total - p.blocks_done) * dt) / db;
}

let inited = false;

/** Our own text in `engine.volumesError` (not one the service sent), kept as a
 *  function: the field stays a string for whoever shows it, and a change of
 *  language writes it again in the new one. */
let volumesErrorText: (() => string) | null = null;

/** Told once, by the first progress event of the running verify, that it is
 *  under way — for an encrypted copy that means the password was accepted,
 *  since nothing is read before the key opens. */
let verifyStarted: (() => void) | null = null;

export async function initEngine(): Promise<void> {
  if (inited) return;
  inited = true;

  onLocale(() => {
    if (volumesErrorText) engine.volumesError = volumesErrorText();
  });

  await listen<EngineProgress>('restore-progress', (e) => {
    engine.restoreProgress = e.payload;
    const now = Date.now();
    const s = restoreClock.samples;
    if (s.length === 0) restoreClock.startMs = now;
    if (s.length === 0 || s[s.length - 1].blocks !== e.payload.blocks_done) {
      s.push({ t: now, blocks: e.payload.blocks_done });
      while (s.length > 2 && s[0].t < now - 12000) s.shift();
    }
  });
  await listen<EngineProgress>('verify-progress', (e) => {
    engine.verifyProgress = e.payload;
    const started = verifyStarted;
    verifyStarted = null;
    started?.();
  });

  await refreshVolumes();
  setInterval(pollDrives, 3000);
}

async function pollDrives(): Promise<void> {
  if (engine.volumesLoading) return;
  try {
    const sig = await api.driveSignature();
    if (sig !== engine.driveSignature) {
      engine.driveSignature = sig;
      await refreshVolumes();
    }
  } catch {
    /* transient */
  }
}

/** A refresh asked for while another was already running. The one in flight may
 *  have started *before* whatever changed the disks — so the request is
 *  remembered instead of dropped. Dropping it is how a restore that repartitioned a disk left the map
 *  showing the layout from before it. */
let refreshAgain = false;

export async function refreshVolumes(): Promise<void> {
  if (engine.volumesLoading) {
    refreshAgain = true;
    return;
  }
  engine.volumesLoading = true;
  engine.volumesError = null;
  volumesErrorText = null;
  try {
    const disks: DiskInfo[] = await api.listDisks();
    // Show disks in physical-drive order (0, 1, 2, …) rather than WMI order.
    const diskNumber = (d: DiskInfo): number => {
      const num = d.device_id.match(/(\d+)\s*$/);
      return num ? parseInt(num[1], 10) : 0;
    };
    engine.disks = [...disks].sort((a, b) => diskNumber(a) - diskNumber(b));
    engine.volumes = disks
      .flatMap((d) =>
        d.partitions
          .filter((p) => p.volume_letter)
          .map((p) => ({
            device: `${p.volume_letter}:`,
            label: `${p.volume_letter}: ${p.volume_label || p.filesystem} — ${formatBytes(p.size_bytes)}`,
          })),
      )
      .sort((a, b) => a.device.localeCompare(b.device));
    engine.volumesLoaded = true;
    try {
      engine.driveSignature = await api.driveSignature();
    } catch {
      /* keep */
    }
    if (engine.volumes.length === 0) {
      volumesErrorText = m.eng_no_volumes;
      engine.volumesError = volumesErrorText();
    }
  } catch (e) {
    engine.volumesError = String(e);
  } finally {
    engine.volumesLoading = false;
  }
  if (refreshAgain) {
    refreshAgain = false;
    await refreshVolumes();
  }
}

export function volumeLabel(device: string): string {
  return engine.volumes.find((v) => v.device === device)?.label ?? device;
}

/** What the person typed to open each encrypted copy, for this session only.
 *
 *  Deliberately in memory and nowhere else. The service will not remember a
 *  password for manual work either (§7.4): the key it holds is for scheduled
 *  COPIES, and handing data back is a separate decision made by whoever is at
 *  the screen. Kept per image because the restore designer reads a copy several
 *  times — describe it, plan against it, execute — and asking three times for
 *  the same password would be a worse kind of security theatre.
 */
const archiveSecrets = new Map<string, ArchiveSecret>();

const secretKey = (imagePath: string) => imagePath.toLowerCase();

export function rememberArchiveSecret(imagePath: string, secret: ArchiveSecret): void {
  archiveSecrets.set(secretKey(imagePath), secret);
}

export function archiveSecretFor(imagePath: string): ArchiveSecret | undefined {
  return archiveSecrets.get(secretKey(imagePath));
}

/** Forget them all — for a "lock" action, or when the window is done. */
export function forgetArchiveSecrets(): void {
  archiveSecrets.clear();
}

/** Does this copy need a password before anything is done to it?
 *
 *  Asked BEFORE the operation, not after its refusal: a restore that gets far
 *  enough to fail has already begun taking the target apart. An unreadable
 *  file answers `false` — whatever is wrong with it is the operation's to
 *  report, not this check's. */
export async function needsArchiveKey(imagePath: string): Promise<boolean> {
  try {
    return (await api.inspectImage(imagePath)).encrypted;
  } catch {
    return false;
  }
}

export async function runRestore(
  imagePath: string,
  target: string,
  secret?: ArchiveSecret,
): Promise<void> {
  if (engine.op) throw new Error(m.eng_busy());
  engine.op = 'restore';
  engine.restoreProgress = null;
  engine.restoreSummary = null;
  engine.restoreError = null;
  restoreClock.startMs = 0;
  restoreClock.samples = [];
  try {
    engine.restoreSummary = await api.restoreNow(imagePath, target, secret);
  } catch (e) {
    engine.restoreError = String(e);
  } finally {
    engine.op = null;
    engine.restoreProgress = null;
  }
}

/** `onStarted` is called once blocks are being read — before the verify
 *  ends, which on a big chain is many minutes later. */
export async function runVerify(
  imagePath: string,
  secret?: ArchiveSecret,
  onStarted?: () => void,
): Promise<VerifySummary> {
  if (engine.op) throw new Error(m.eng_busy());
  engine.op = 'verify';
  engine.verifyProgress = null;
  verifyStarted = onStarted ?? null;
  try {
    return await api.verifyNow(imagePath, secret);
  } finally {
    verifyStarted = null;
    engine.op = null;
    engine.verifyProgress = null;
  }
}

export async function cancelCurrent(): Promise<void> {
  try {
    await api.cancelEngineOp();
  } catch {
    /* ignore */
  }
}

export function clearRestoreResult(): void {
  engine.restoreSummary = null;
  engine.restoreError = null;
}

export const toolsState = $state({
  mountPath: "",
  rwMode: false,
  verifyPath: "",
  /** Set by whoever sends the person to «Инструменты» with an image already
   *  chosen: the page mounts `mountPath` once, as if the button were pressed. */
  autoMount: false,
});
