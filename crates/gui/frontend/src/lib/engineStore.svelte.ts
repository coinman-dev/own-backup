// Shared, app-lifetime state for volume enumeration and the Restore/Tools
// engine ops (restore/verify/mount). Backups run through jobStore's queue.

import { listen } from '@tauri-apps/api/event';
import { api } from './api';
import { formatBytes } from './format';
import type { DiskInfo, EngineProgress, RestoreSummary, VerifySummary } from './types';

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

let inited = false;

export async function initEngine(): Promise<void> {
  if (inited) return;
  inited = true;

  await listen<EngineProgress>('restore-progress', (e) => {
    engine.restoreProgress = e.payload;
  });
  await listen<EngineProgress>('verify-progress', (e) => {
    engine.verifyProgress = e.payload;
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

export async function refreshVolumes(): Promise<void> {
  if (engine.volumesLoading) return;
  engine.volumesLoading = true;
  engine.volumesError = null;
  try {
    const disks: DiskInfo[] = await api.listDisks();
    // Show disks in physical-drive order (0, 1, 2, …) rather than WMI order.
    const diskNumber = (d: DiskInfo): number => {
      const m = d.device_id.match(/(\d+)\s*$/);
      return m ? parseInt(m[1], 10) : 0;
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
      engine.volumesError =
        'Не найдено ни одного тома с буквой. Подробности — в own-backup-gui.log рядом с exe.';
    }
  } catch (e) {
    engine.volumesError = String(e);
  } finally {
    engine.volumesLoading = false;
  }
}

export function volumeLabel(device: string): string {
  return engine.volumes.find((v) => v.device === device)?.label ?? device;
}

export async function runRestore(imagePath: string, target: string): Promise<void> {
  if (engine.op) throw new Error('Уже выполняется другая операция.');
  engine.op = 'restore';
  engine.restoreProgress = null;
  engine.restoreSummary = null;
  engine.restoreError = null;
  try {
    engine.restoreSummary = await api.restoreNow(imagePath, target);
  } catch (e) {
    engine.restoreError = String(e);
  } finally {
    engine.op = null;
    engine.restoreProgress = null;
  }
}

export async function runVerify(imagePath: string): Promise<VerifySummary> {
  if (engine.op) throw new Error('Уже выполняется другая операция.');
  engine.op = 'verify';
  engine.verifyProgress = null;
  try {
    return await api.verifyNow(imagePath);
  } finally {
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
});
