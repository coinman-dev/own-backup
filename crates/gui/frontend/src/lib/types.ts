export interface DiskInfo {
  device_id: string;
  model: string;
  serial: string;
  size_bytes: number;
  sector_size_logical: number;
  sector_size_physical: number;
  media_type: string;
  partition_table: string;
  disk_guid: string;
  partitions: PartitionInfo[];
}

export interface PartitionInfo {
  index: number;
  offset_bytes: number;
  size_bytes: number;
  /** Bytes in use within the volume (size − free); 0 when unknown/letterless. */
  used_bytes: number;
  gpt_type_guid: string;
  gpt_partition_guid: string;
  filesystem: string;
  volume_letter: string | null;
  /** Openable device: "C:" for lettered, `\\?\Volume{GUID}` for letterless
   *  system/hidden volumes (EFI, Recovery), "" when not backable (MSR, Linux). */
  volume_device: string;
  volume_label: string;
  fs_uuid: string;
  is_system: boolean;
  is_boot: boolean;
  /** Encrypted container (LUKS/BitLocker): filesystem is Unknown, but it can be
   *  backed up after supplying a password to decrypt it. */
  is_encrypted: boolean;
}

export interface BackupJob {
  id: string;
  name: string;
  source_type: string;
  source_device: string;
  target_path: string;
  schedule: string | null;
  compression_level: number;
  encryption_enabled: boolean;
  created_at: string;
  updated_at: string;
}

export interface BackupSet {
  id: string;
  job_id: string;
  backup_type: string;
  parent_set_id: string | null;
  file_path: string;
  file_size: number | null;
  source_size: number;
  block_count: number;
  changed_block_count: number | null;
  started_at: string | null;
  completed_at: string | null;
  status: string;
  checksum: string | null;
  manifest_json: string | null;
}

export interface BackupProgress {
  backup_id: string;
  status: string;
  blocks_done: number;
  blocks_total: number;
  bytes_written: number;
  speed_mbps: number;
  eta: string;
  error_message: string;
}

export interface PreflightReport {
  boot_mode: string;
  partition_table: string;
  disk_type: string;
  filesystem: string;
  support_tier: string;
  vss_service_running: boolean;
  vss_writers_healthy: boolean;
  free_space_on_target: number;
  warnings: string[];
  blockers: string[];
}

export interface RetentionPolicy {
  job_id: string;
  max_full_backups: number | null;
  max_incremental_per_full: number | null;
  max_age_days: number | null;
  min_keep_count: number;
}

export interface ValidationResult {
  valid: boolean;
  message: string;
  blocks_checked: number;
  blocks_total: number;
}

export interface ServiceStatus {
  connected: boolean;
  version: string;
  uptime: string;
  active_jobs: number;
  total_backups_completed: number;
}

export type ViewName =
  | 'backup'
  | 'restore'
  | 'tools'
  | 'activity'
  | 'settings'
  | 'about';

export interface EngineProgress {
  percent: number;
  blocks_done: number;
  blocks_total: number;
  bytes_done: number;
  speed_mbps: number;
}

export interface BackupSummary {
  image_path: string;
  sidecar_path: string;
  source_bytes: number;
  total_blocks: number;
  used_blocks: number;
  bytes_copied: number;
  duration_secs: number;
  smart_mode: boolean;
  vss_used: boolean;
}

export interface RestoreSummary {
  blocks_restored: number;
  bytes_restored: number;
  duration_secs: number;
}

export interface VerifySummary {
  valid: boolean;
  blocks_checked: number;
  mismatched_blocks: number[];
  duration_secs: number;
}

export interface ImageInfo {
  image_file_name: string;
  chain_role: string;
  created_at: string;
  source_device: string;
  source_bytes: number;
  block_size: number;
  used_blocks: number;
  total_blocks: number;
  backup_mode: string;
}

// ===== Job model (phase 6) =====

export interface ScheduleConfig {
  kind: 'none' | 'daily' | 'weekly' | 'monthly' | 'event' | 'nonstop';
  days: string[];
  time: string;
  only_when_locked: boolean;
  wake_to_run: boolean;
  prevent_sleep: boolean;
  run_missed: boolean;
  missed_delay_min: number;
}

export interface SchemeConfig {
  method: 'full' | 'incremental' | 'differential';
  only_incremental: boolean;
  full_every_n: number;
  cleanup_older_than_days: number | null;
  keep_chains: number | null;
  keep_first: boolean;
}

export interface ExclusionsConfig {
  enabled: boolean;
  patterns: string[];
}

export interface AdvancedConfig {
  sector_by_sector: boolean;
  validate_after: boolean;
  backup_unallocated: boolean;
  password_enabled: boolean;
  pre_command: string;
  post_command: string;
  error_handling: string;
  shutdown_after: boolean;
  performance: string;
}

/** Byte totals per file category — real data from the backup walk. */
export interface CategorySizes {
  images: number;
  video: number;
  audio: number;
  documents: number;
  system: number;
  other: number;
}

export interface JobRun {
  id: string;
  job_id: string;
  job_name: string;
  started_at: string;
  finished_at: string;
  status: 'success' | 'error' | 'cancelled' | string;
  image_path: string;
  source_bytes: number;
  bytes_copied: number;
  used_blocks: number;
  total_blocks: number;
  duration_secs: number;
  smart_mode: boolean;
  vss_used: boolean;
  error: string;
  /** full | incremental | differential; '' in pre-phase-5 runs (= full) */
  backup_kind: string;
  base_image_path: string;
  increment_index: number;
  blocks_unchanged: number;
  excluded_files: number;
  excluded_bytes: number;
  /** null when the walk did not run (exclusions off / non-NTFS / sector mode) */
  categories: CategorySizes | null;
}

/** One partition of a whole-disk backup: its GPT slot and source device. */
export interface DiskPartSpec {
  part_index: number;
  device: string;
  filesystem: string;
}

/** Whole-disk (bootable) backup metadata attached to a job. */
export interface DiskSourceSpec {
  disk_number: number;
  disk_model: string;
  parts: DiskPartSpec[];
}

export interface Job {
  id: string;
  name: string;
  /** Primary source volume, e.g. "E:" (used for file naming / back-compat). */
  source: string;
  source_label: string;
  /** All selected source volumes; each becomes its own image in one run. */
  sources: string[];
  source_labels: string[];
  /** At least one selected source is encrypted (needs a password to back up). */
  requires_password: boolean;
  /** Set for a whole-disk (bootable) backup: which disk + how partitions map. */
  disk_source: DiskSourceSpec | null;
  dest_folder: string;
  schedule: ScheduleConfig;
  scheme: SchemeConfig;
  exclusions: ExclusionsConfig;
  advanced: AdvancedConfig;
  created_at: string;
  updated_at: string;
  last_run: JobRun | null;
}

/** Result of decrypting an encrypted source in the wizard. */
export interface DecryptResult {
  ok: boolean;
  filesystem: string;
  label: string;
  used_bytes: number;
}

export interface QueueSnapshot {
  current: string | null;
  pending: string[];
}

export interface JobProgress {
  job_id: string;
  /**
   * 'prepare'  = exclusion scan (blocks_done counts scanned entries)
   * 'backup'   = copying blocks
   * 'finalize' = flushing the image to disk / detaching (no granular progress)
   * 'verify'   = post-backup validation
   */
  phase: 'prepare' | 'backup' | 'finalize' | 'verify' | string;
  percent: number;
  blocks_done: number;
  blocks_total: number;
  speed_mbps: number;
  /** 0-based index of the volume being copied in a multi-volume job. */
  volume_index?: number;
  /** Total volumes in this run (1 for single-volume jobs). */
  volume_count?: number;
  /** Label of the current volume, e.g. "C: Windows". */
  volume?: string;
}
