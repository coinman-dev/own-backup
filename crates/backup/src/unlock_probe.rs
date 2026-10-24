//! What a passphrase opened: whether it unlocked an encrypted volume, and the
//! filesystem, label and used bytes inside — so the interface can show it like
//! any other volume. Shared by the Windows and Linux engines; the platform part
//! is only how the encrypted bytes are reached.

use own_backup_common::Result;
use tracing::warn;

/// Outcome of decrypting an encrypted source: whether the passphrase unlocked
/// it, and (on success) the inner filesystem's name / volume label / used bytes
/// so the UI can show them like any other volume.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct DecryptedProbe {
    pub ok: bool,
    pub filesystem: String,
    pub label: String,
    pub used_bytes: u64,
    /// Where the filesystem INSIDE the container says it was last mounted
    /// (ext only). This is what turns "the password opened something" into an
    /// answer to the question the wizard actually asked: is the system in here,
    /// or is this a data volume the user merely wanted to look at?
    pub linux_last_mounted: String,
}

/// Describe what is inside a container we have just unlocked: filesystem,
/// volume label, bytes in use.
///
/// The superblock alone answers this for ext/xfs/btrfs/FAT — their free-space
/// counters and labels live there. NTFS keeps neither: its label is in the MFT
/// and its free space in `$Bitmap`. Windows would know both, but only for a
/// volume it has mounted, and this one it cannot mount — so both are read here,
/// from the same structures the used-block map is built from. A failure there
/// costs the numbers, not the unlock.
pub fn probe_decrypted(view: &mut dyn crate::fat_map::RawVolume) -> Result<DecryptedProbe> {
    use own_backup_common::types::FileSystem;

    let want = (own_backup_inventory::fs_probe::PROBE_LEN as u64).min(view.len()) as usize;
    let mut buf = vec![0u8; want];
    view.read_at(0, &mut buf)?;
    let Some(probed) = own_backup_inventory::fs_probe::identify(&buf) else {
        return Ok(DecryptedProbe {
            ok: true,
            ..Default::default()
        });
    };
    let filesystem = format!("{:?}", probed.fs).to_lowercase();
    let mut label = probed.label.unwrap_or_default();
    let mut used_bytes = probed.used_bytes.unwrap_or(0);

    if probed.fs == FileSystem::Ntfs {
        match crate::ntfs_map::ntfs_used_bytes(view) {
            Ok(used) => used_bytes = used,
            Err(e) => warn!(error = %e, "could not count used bytes of the unlocked NTFS volume"),
        }
        if label.is_empty() {
            label = crate::ntfs_map::ntfs_volume_label(view).unwrap_or_default();
        }
    }
    Ok(DecryptedProbe {
        ok: true,
        filesystem,
        label,
        used_bytes,
        linux_last_mounted: probed.last_mounted.unwrap_or_default(),
    })
}
