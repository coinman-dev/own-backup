// Messages in Russian and English (locales/*.json), through
// own_backup_common::tr! — see common/src/lang.rs.
rust_i18n::i18n!("locales", fallback = "ru");

pub mod aligned_volume;
pub mod bcd;
pub mod bitlocker;
pub mod block_reader;
pub mod block_source;
pub mod boot_code;
pub mod btrfs_exclusions;
pub mod btrfs_map;
pub mod chains;
pub mod checksum;
pub mod consolidate;
pub mod decrypt;
pub mod dest_folder;
pub mod efivars;
// Shared by both engines.
mod engine_core;
pub mod esp_fallback;
pub mod esp_scan;
pub mod exclusions;
pub mod export;
pub mod ext_exclusions;
pub mod ext_map;
pub mod fat_exclusions;
pub mod fat_map;
pub mod fs_uuid;
pub mod gpt_reader;
pub mod gpt_writer;
pub mod identity_clash;
pub mod iscsi;
pub mod luks;
pub mod mbr_writer;
pub mod ntfs_map;
pub mod ntfs_scan;
pub mod overlay;
pub mod pending_image;
pub mod qcow2;
pub mod raw_copy;
pub mod regf;
pub mod registry_win;
pub mod removal_types;
pub mod restore_map;
pub mod san_policy;
/// Шифрование копий: ключи задания, ключ цепочки, шифр кластеров (`.ai/PLAN.md` §7).
pub mod seal;
pub mod secrets;
pub mod sector_restore;
pub mod sidecar;
pub mod source_identity;
pub mod table_merge;
pub mod table_writer;
pub mod unlock_probe;
pub mod used_sectors;
pub mod vhdx_bat;
pub mod vhdx_chain;
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
pub mod disk_session_linux;
#[cfg(target_os = "linux")]
pub mod efivars_linux;
#[cfg(target_os = "linux")]
pub mod engine_linux;
#[cfg(target_os = "linux")]
pub mod identity_check_linux;
// The engine's Linux platform: the volume pipeline runs on it.
#[cfg(target_os = "linux")]
mod engine_platform_linux;
#[cfg(target_os = "linux")]
pub mod linux_device;
#[cfg(target_os = "linux")]
pub mod nbd_linux;
#[cfg(target_os = "linux")]
pub mod udev_hold_linux;
pub mod vhdx_format;

pub mod attach_win;
#[cfg(target_os = "windows")]
#[cfg(target_os = "windows")]
#[cfg(target_os = "windows")]
pub mod automount;
#[cfg(target_os = "windows")]
pub mod block_reader_win;
#[cfg(target_os = "windows")]
pub mod disk_session_win;
#[cfg(target_os = "windows")]
pub mod efivars_win;
#[cfg(target_os = "windows")]
pub mod engine_win;
// The engine's Windows platform: the volume pipeline runs on it.
#[cfg(target_os = "windows")]
mod engine_platform_win;
#[cfg(target_os = "windows")]
pub mod esp_fallback_win;
#[cfg(target_os = "windows")]
pub mod esp_scan_win;
#[cfg(target_os = "windows")]
pub mod exclusions_win;
pub mod fat_writer;
#[cfg(target_os = "windows")]
pub mod file_restore_win;
#[cfg(target_os = "windows")]
pub mod format_win;
pub mod iscsi_win;
pub mod iso9660;
#[cfg(target_os = "windows")]
pub mod letters_win;
#[cfg(target_os = "windows")]
pub mod media_build_win;
pub mod media_layout;
pub mod media_preflight_win;
#[cfg(target_os = "windows")]
pub mod media_tools_win;
#[cfg(target_os = "windows")]
pub mod obk_removal_win;
#[cfg(target_os = "linux")]
pub mod power_linux;
#[cfg(target_os = "windows")]
pub mod power_win;
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
// Mostly Windows-only, but its INF parsing is plain text and its tests run
// everywhere — the module is compiled on all platforms, the process calls are
// gated inside it.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub mod media_guest_drivers_win;
pub mod ole_compound;
#[cfg(target_os = "windows")]
pub mod volume_win;
pub mod volume_window;
pub mod wim_info;
#[cfg(target_os = "windows")]
pub mod win_device;
#[cfg(target_os = "windows")]
pub mod windows_unlock_win;
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub mod winre_find;

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
