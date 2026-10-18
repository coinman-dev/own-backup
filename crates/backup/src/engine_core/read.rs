//! Reading a finished image back in userspace, whatever its container, and
//! reading the sidecar that describes it.

use std::path::Path;

use own_backup_common::{tr, OwnBackupError, Result};

use crate::sidecar::Sidecar;

/// Read source behind the same format switch, picked by the sidecar's
/// `container` field. BOTH formats are read in userspace now — no attach.
///
/// Reading a VHDX used to attach it read-only, and that attach was the last
/// place an image could still be mounted by Windows. `NO_DRIVE_LETTER`
/// withholds the letter, not the mount: the volumes surfaced anyway, and the
/// detach afterwards waited out the ~180 s PnP removal veto (measured
/// 2026-08-21: `verify: image OK` at 15:43:20, the backup "finished" at
/// 15:46:20). The write path can dismiss those volumes because it can take the
/// disk offline; a read attach has no writable handle to do that with, so the
/// volume comes straight back and the veto is unavoidable.
///
/// So the read path stopped attaching. `vhdx_bat::VhdxGuestReader` resolves the
/// differencing chain from the files themselves — the same reader the embedded
/// sidecar has always been read with — which makes VHDX reads work exactly like
/// qcow2 reads: plain file I/O, no device, no volume, nothing to detach, and
/// nothing that depends on a SAN policy taking effect. It is also what the
/// measurements said to expect: ~260 s for 3.4 GiB either way (CLAUDE.md).
pub(crate) enum ImageReadSource {
    Vhdx(Box<crate::vhdx_bat::VhdxGuestReader>),
    Qcow2(crate::qcow2::Qcow2RandomReader),
    Obk(Box<crate::seal::obk::ObkRandomReader>),
}

impl ImageReadSource {
    pub(crate) fn open(image_path: &Path, container: &str) -> Result<Self> {
        Self::open_with(image_path, container, None)
    }

    /// `key` opens an encrypted image and its whole chain. Without it an `.obk`
    /// is refused by name rather than half-read.
    pub(crate) fn open_with(
        image_path: &Path,
        container: &str,
        key: Option<&crate::seal::ChainKey>,
    ) -> Result<Self> {
        match (container, key) {
            ("qcow2", _) => Ok(Self::Qcow2(crate::qcow2::Qcow2RandomReader::open(
                image_path,
            )?)),
            ("vhdx", _) => Ok(Self::Vhdx(Box::new(
                crate::vhdx_bat::VhdxGuestReader::open(image_path)?,
            ))),
            ("obk", Some(key)) => Ok(Self::Obk(Box::new(
                crate::seal::obk::ObkRandomReader::open(image_path, key)?,
            ))),
            ("obk", None) => Err(OwnBackupError::Encryption(
                tr!(
                    "engine_win.encrypted_needs_key",
                    image = image_path.display()
                )
                .into_owned(),
            )),
            (other, _) => Err(OwnBackupError::ImageFormat(format!(
                "unknown image container '{other}' in sidecar"
            ))),
        }
    }

    pub(crate) fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        match self {
            Self::Vhdx(r) => r.read_at(offset, buf),
            Self::Qcow2(r) => r.read_at(offset, buf),
            Self::Obk(r) => r.read_at(offset, buf),
        }
    }
}

/// Read an image's sidecar: embedded first, adjacent `.obkm` as a fallback
/// for images written before the sidecar moved inside the container.
pub fn load_sidecar_of(image_path: &Path) -> Result<Sidecar> {
    load_sidecar_with_keys(image_path, None)
}

/// The same, able to open an encrypted image. Its sidecar is sealed inside the
/// container, so there is nothing to read without the key — and saying which
/// key is missing beats failing with "unsupported container".
pub fn load_sidecar_with_keys(
    image_path: &Path,
    keys: Option<&crate::seal::context::ArchiveKeys>,
) -> Result<Sidecar> {
    // One reader for the whole product; this name stays because the Windows
    // engine is full of callers of it.
    crate::sidecar::read_sidecar_with_keys(image_path, keys)
}
