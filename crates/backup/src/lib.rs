pub mod block_reader;
pub mod checksum;
pub mod compression;
pub mod encryption;
pub mod image_reader;
pub mod image_writer;
pub mod incremental;
pub mod pipeline;
pub mod used_sectors;

#[cfg(target_os = "windows")]
pub mod block_reader_win;
#[cfg(target_os = "windows")]
pub mod used_sectors_win;

pub use block_reader::{BlockReader, MockBlockReader};
pub use checksum::compute_block_checksum;
pub use compression::{compress_block, decompress_block};
pub use encryption::{decrypt_block, derive_key, encrypt_block, generate_salt, EncryptionKey};
pub use image_reader::ImageReader;
pub use image_writer::ImageWriter;
pub use incremental::IncrementalEngine;
pub use pipeline::BackupPipeline;
pub use used_sectors::{AllUsedMap, UsedSectorMap};
