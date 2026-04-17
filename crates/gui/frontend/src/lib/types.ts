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
  | 'dashboard'
  | 'activity'
  | 'restore'
  | 'job-settings'
  | 'settings'
  | 'about';
