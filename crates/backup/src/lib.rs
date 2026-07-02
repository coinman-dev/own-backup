pub mod block_reader;
pub mod checksum;
pub mod compression;
pub mod encryption;
pub mod gpt_writer;
pub mod image_reader;
pub mod image_writer;
pub mod incremental;
pub mod pipeline;
pub mod raw_copy;
pub mod sidecar;
pub mod used_sectors;

#[cfg(target_os = "windows")]
pub mod block_reader_win;
#[cfg(target_os = "windows")]
pub mod engine_win;
#[cfg(target_os = "windows")]
pub mod used_sectors_win;
#[cfg(target_os = "windows")]
pub mod vhdx;
#[cfg(target_os = "windows")]
pub mod win_device;

pub use block_reader::{BlockReader, MockBlockReader};
pub use checksum::compute_block_checksum;
pub use compression::{compress_block, decompress_block};
pub use encryption::{decrypt_block, derive_key, encrypt_block, generate_salt, EncryptionKey};
pub use image_reader::ImageReader;
pub use image_writer::ImageWriter;
pub use incremental::IncrementalEngine;
pub use pipeline::BackupPipeline;
pub use raw_copy::{copy_used_blocks, CopyOutcome};
pub use sidecar::{
    read_sidecar, sidecar_path_for, write_sidecar, BlockChecksum, ChainRole, Sidecar, SidecarInfo,
};
pub use used_sectors::{AllUsedMap, UsedSectorMap};

#[cfg(target_os = "windows")]
pub use block_reader_win::WinBlockReader;
#[cfg(target_os = "windows")]
pub use used_sectors_win::NtfsUsedSectorMap;
#[cfg(target_os = "windows")]
pub use win_device::{normalize_device_path, WinRawDevice};
