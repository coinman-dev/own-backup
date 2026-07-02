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
  gpt_type_guid: string;
  gpt_partition_guid: string;
  filesystem: string;
  volume_letter: string | null;
  volume_label: string;
  fs_uuid: string;
  is_system: boolean;
  is_boot: boolean;
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
}

export interface Job {
  id: string;
  name: string;
  source: string;
  source_label: string;
  dest_folder: string;
  schedule: ScheduleConfig;
  scheme: SchemeConfig;
  exclusions: ExclusionsConfig;
  advanced: AdvancedConfig;
  created_at: string;
  updated_at: string;
  last_run: JobRun | null;
}

export interface QueueSnapshot {
  current: string | null;
  pending: string[];
}

export interface JobProgress {
  job_id: string;
  phase: 'backup' | 'verify' | string;
  percent: number;
  blocks_done: number;
  blocks_total: number;
  speed_mbps: number;
}
