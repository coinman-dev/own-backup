import type { BackupJob, BackupProgress, DiskInfo, ServiceStatus } from './types';

/**
 * Shared reactive state using Svelte 5 module-level $state.
 *
 * Import this module wherever you need access to global app state.
 * Because $state is reactive at the module level in Svelte 5,
 * any component that reads these values will re-render when they change.
 */

/** All backup jobs known to the frontend. */
export let jobs: BackupJob[] = $state([]);

/** Detected disks from the backend. */
export let disks: DiskInfo[] = $state([]);

/** Active backup progress updates keyed by backup_id. */
export let activeProgress: Record<string, BackupProgress> = $state({});

/** Backend service status, if available. */
export let serviceStatus: ServiceStatus | null = $state(null);

/** Whether the app is currently loading initial data. */
export let isLoading: boolean = $state(false);

/** Global error message to display, if any. */
export let globalError: string | null = $state(null);

/** The currently selected job ID (for job-settings view). */
export let selectedJobId: string | null = $state(null);
