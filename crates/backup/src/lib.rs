pub mod bitlocker;
pub mod block_reader;
pub mod block_source;
pub mod btrfs_map;
pub mod chains;
pub mod checksum;
pub mod compression;
pub mod decrypt;
pub mod devio;
pub mod encryption;
pub mod exclusions;
pub mod ext_exclusions;
pub mod ext_map;
pub mod fat_exclusions;
pub mod fat_map;
pub mod gpt_reader;
pub mod gpt_writer;
pub mod image_reader;
pub mod image_writer;
pub mod incremental;
pub mod luks;
pub mod ntfs_map;
pub mod overlay;
pub mod pipeline;
pub mod qcow2;
pub mod raw_copy;
pub mod san_policy;
pub mod secrets;
pub mod sidecar;
pub mod used_sectors;
pub mod vhdx_bat;
pub mod xfs_map;

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
pub mod engine_win;
#[cfg(target_os = "windows")]
pub mod exclusions_win;
#[cfg(target_os = "windows")]
pub mod privileges_win;
#[cfg(target_os = "windows")]
pub mod qcow2_removal_win;
#[cfg(target_os = "windows")]
pub mod true_removal_win;
#[cfg(target_os = "windows")]
pub mod used_sectors_win;
#[cfg(target_os = "windows")]
pub mod vhdx;
#[cfg(target_os = "windows")]
pub mod win_device;

pub use block_reader::{BlockReader, MockBlockReader};
pub use chains::{delete_chain, list_job_chains, plan_retention, Chain, RetentionRules};
pub use checksum::compute_block_checksum;
pub use compression::{compress_block, decompress_block};
pub use encryption::{decrypt_block, derive_key, encrypt_block, generate_salt, EncryptionKey};
pub use exclusions::{parse_patterns, ByteRanges, ExclusionPattern, ZeroingBlockReader};
pub use image_reader::ImageReader;
pub use image_writer::ImageWriter;
pub use incremental::IncrementalEngine;
pub use pipeline::BackupPipeline;
pub use qcow2::{Qcow2Compression, Qcow2Reader, Qcow2Writer, Qcow2WriterOptions};
pub use raw_copy::{copy_used_blocks, CopyOutcome};
pub use sidecar::{
    image_sidecar_readable, read_embedded_sidecar, read_embedded_sidecar_header, read_sidecar,
    read_sidecar_header, sidecar_path_for, write_sidecar, BlockChecksum, ChainRole, Sidecar,
    SidecarInfo,
};
pub use used_sectors::{AllUsedMap, UsedSectorMap};

#[cfg(target_os = "windows")]
pub use block_reader_win::WinBlockReader;
#[cfg(target_os = "windows")]
pub use used_sectors_win::NtfsUsedSectorMap;
#[cfg(target_os = "windows")]
pub use win_device::{normalize_device_path, WinRawDevice};
