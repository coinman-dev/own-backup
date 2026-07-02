// Shared, app-lifetime state for the backup engine.
//
// Why this exists: views are unmounted when the user switches tabs, so any
// state (volume list, running-operation progress, results) held inside a view
// component is lost — and re-scanned on return. This module holds that state
// at app scope and drives long operations from here, so a backup keeps
// reporting progress across tab switches and a summary survives navigation.

import { listen } from '@tauri-apps/api/event';
import { api } from './api';
import { formatBytes } from './format';
import type {
  DiskInfo,
  EngineProgress,
  BackupSummary,
  RestoreSummary,
  VerifySummary,
} from './types';

export interface VolumeOption {
  device: string;
  label: string;
}

type OpKind = 'backup' | 'restore' | 'verify';

export const engine = $state({
  // --- volume cache ---
  volumes: [] as VolumeOption[],
  volumesLoaded: false,
  volumesLoading: false,
  volumesError: null as string | null,
  driveSignature: -1,

  // --- global single-operation guard ---
  op: null as OpKind | null,

  // --- backup ---
  backupProgress: null as EngineProgress | null,
  backupSummary: null as BackupSummary | null,
  backupError: null as string | null,
  backupFrom: '',
  backupTo: '',
  backupStartMs: 0,

  // --- restore ---
  restoreProgress: null as EngineProgress | null,
  restoreSummary: null as RestoreSummary | null,
  restoreError: null as string | null,

  // --- verify (shared by Backup screen actions and Tools) ---
  verifyProgress: null as EngineProgress | null,
});

let inited = false;

/** Register event listeners once and load the initial volume list. */
export async function initEngine(): Promise<void> {
  if (inited) return;
  inited = true;

  await listen<EngineProgress>('backup-progress', (e) => {
    engine.backupProgress = e.payload;
  });
  await listen<EngineProgress>('restore-progress', (e) => {
    engine.restoreProgress = e.payload;
  });
  await listen<EngineProgress>('verify-progress', (e) => {
    engine.verifyProgress = e.payload;
  });

  await refreshVolumes();

  // Cheap poll for USB insert/eject — re-enumerate only when the set of drive
  // letters actually changes, so we don't run PowerShell on a timer.
  setInterval(pollDrives, 3000);
}

async function pollDrives(): Promise<void> {
  // Don't disturb an enumeration or a running operation.
  if (engine.volumesLoading) return;
  try {
    const sig = await api.driveSignature();
    if (sig !== engine.driveSignature) {
      engine.driveSignature = sig;
      await refreshVolumes();
    }
  } catch {
    /* transient — ignore */
  }
}

/** Force a full re-enumeration of volumes. */
export async function refreshVolumes(): Promise<void> {
  if (engine.volumesLoading) return;
  engine.volumesLoading = true;
  engine.volumesError = null;
  try {
    const disks: DiskInfo[] = await api.listDisks();
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
      /* keep old signature */
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

/** True if any engine operation is in progress. */
export function busy(): boolean {
  return engine.op !== null;
}

// --- long operations: driven from module scope so they survive tab switches ---

export async function runBackup(
  source: string,
  outputPath: string,
  allSectors: boolean,
  fromLabel: string,
): Promise<void> {
  if (engine.op) throw new Error('Уже выполняется другая операция.');
  engine.op = 'backup';
  engine.backupProgress = null;
  engine.backupSummary = null;
  engine.backupError = null;
  engine.backupFrom = fromLabel;
  engine.backupTo = outputPath;
  engine.backupStartMs = Date.now();
  try {
    engine.backupSummary = await api.backupNow(source, outputPath, allSectors);
  } catch (e) {
    engine.backupError = String(e);
  } finally {
    engine.op = null;
    engine.backupProgress = null;
  }
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

/** Verify runs through the guard too; returns the summary for local display. */
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

export function clearBackupResult(): void {
  engine.backupSummary = null;
  engine.backupError = null;
}

export function clearRestoreResult(): void {
  engine.restoreSummary = null;
  engine.restoreError = null;
}

export type { BackupSummary, RestoreSummary };
