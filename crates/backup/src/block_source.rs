//! The one thing every block-device adapter needs from an image: its size and
//! random reads. Served to the kernel by [`crate::nbd_linux`] on Linux and to
//! Windows' own iSCSI initiator by [`crate::iscsi`] — same providers (qcow2
//! chains with decompression, VHDX) behind both.

use own_backup_common::Result;

pub trait BlockSource: Send {
    fn size(&self) -> u64;
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()>;
}

/// A block source that also accepts writes — served RW so the OS filesystem
/// driver can delete excluded files (NTFS/ReFS true removal for the qcow2
/// container). Writes land in a scratch overlay, never the base image.
pub trait WritableBlockSource: BlockSource {
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()>;
}

/// An encrypted image (with its chain) serves the same way, once its key has
/// opened it. Every cluster is authenticated on the way out, so a mounted
/// `.obk` cannot hand the filesystem driver bytes that are not ours.
impl BlockSource for crate::seal::obk::ObkRandomReader {
    fn size(&self) -> u64 {
        self.virtual_size()
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        crate::seal::obk::ObkRandomReader::read_at(self, offset, buf)
    }
}

/// A qcow2 image (with its backing chain) is a block source directly — this
/// is what the Windows mount path serves, sidecar not required.
impl BlockSource for crate::qcow2::Qcow2RandomReader {
    fn size(&self) -> u64 {
        self.virtual_size()
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        crate::qcow2::Qcow2RandomReader::read_at(self, offset, buf)
    }
}
