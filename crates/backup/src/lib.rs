pub mod bitlocker;
pub mod block_reader;
pub mod block_source;
pub mod btrfs_exclusions;
pub mod btrfs_map;
pub mod chains;
pub mod checksum;
pub mod decrypt;
pub mod devio;
pub mod exclusions;
pub mod esp_fallback;
pub mod ext_exclusions;
pub mod ext_map;
pub mod fat_exclusions;
pub mod aligned_volume;
pub mod fat_map;
pub mod fs_uuid;
pub mod gpt_reader;
pub mod gpt_writer;
pub mod luks;
pub mod mbr_writer;
pub mod ntfs_map;
pub mod ntfs_scan;
pub mod overlay;
pub mod qcow2;
pub mod raw_copy;
pub mod restore_map;
pub mod registry_win;
pub mod san_policy;
pub mod secrets;
pub mod sidecar;
pub mod table_merge;
pub mod table_writer;
pub mod used_sectors;
pub mod vhdx_bat;
pub mod xfs_dirent;
pub mod xfs_efree;
pub mod xfs_exclusions;
pub mod xfs_ifree;
pub mod xfs_map;
pub mod xfs_stage2;

/// The shared fixture zoo for the root-gated dynamic acceptance (этап T).
#[cfg(all(test, target_os = "linux"))]
pub mod exclusion_corpus;

#[cfg(target_os = "linux")]
pub mod engine_linux;
#[cfg(target_os = "linux")]
pub mod linux_device;
#[cfg(target_os = "linux")]
pub mod nbd_linux;
#[cfg(target_os = "linux")]
pub mod vhdx_format;

#[cfg(target_os = "windows")]
pub mod aim_install;
#[cfg(target_os = "windows")]
pub mod aim_win;
#[cfg(target_os = "windows")]
pub mod automount;
#[cfg(target_os = "windows")]
pub mod block_reader_win;
#[cfg(target_os = "windows")]
pub mod disk_session_win;
#[cfg(target_os = "windows")]
pub mod engine_win;
#[cfg(target_os = "windows")]
pub mod esp_fallback_win;
#[cfg(target_os = "windows")]
pub mod exclusions_win;
#[cfg(target_os = "windows")]
pub mod file_restore_win;
#[cfg(target_os = "windows")]
pub mod format_win;
#[cfg(target_os = "windows")]
pub mod letters_win;
#[cfg(target_os = "windows")]
pub mod privileges_win;
#[cfg(target_os = "windows")]
pub mod qcow2_removal_win;
#[cfg(target_os = "windows")]
pub mod restore_win;
#[cfg(target_os = "windows")]
pub mod true_removal_win;
#[cfg(target_os = "windows")]
pub mod used_sectors_win;
#[cfg(target_os = "windows")]
pub mod vhdx;
pub mod fat_writer;
pub mod iso9660;
pub mod media_layout;
pub mod media_preflight_win;
#[cfg(target_os = "windows")]
pub mod media_build_win;
// Mostly Windows-only, but its INF parsing is plain text and its tests run
// everywhere — the module is compiled on all platforms, the process calls are
// gated inside it.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub mod media_guest_drivers_win;
pub mod ole_compound;
pub mod volume_window;
pub mod wim_info;
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub mod winre_find;
#[cfg(target_os = "windows")]
pub mod volume_win;
#[cfg(target_os = "windows")]
pub mod win_device;
#[cfg(target_os = "windows")]
pub mod windows_unlock_win;

pub use block_reader::{BlockReader, MockBlockReader};
pub use chains::{delete_chain, list_job_chains, plan_retention, Chain, RetentionRules};
pub use checksum::compute_block_checksum;
pub use exclusions::{parse_patterns, ByteRanges, ExclusionPattern, ZeroingBlockReader};
pub use qcow2::{Qcow2Compression, Qcow2Reader, Qcow2Writer, Qcow2WriterOptions};
pub use raw_copy::{copy_used_blocks, CopyOutcome};
pub use sidecar::{
    detect_container_format, image_sidecar_readable, read_embedded_sidecar,
    read_embedded_sidecar_header, read_sidecar, read_sidecar_header, sidecar_path_for,
    write_sidecar, BlockChecksum, ChainRole, Sidecar, SidecarInfo,
};
pub use used_sectors::{AllUsedMap, UsedSectorMap};

#[cfg(target_os = "windows")]
pub use block_reader_win::WinBlockReader;
#[cfg(target_os = "windows")]
pub use used_sectors_win::NtfsUsedSectorMap;
#[cfg(target_os = "windows")]
pub use win_device::{normalize_device_path, WinRawDevice};
