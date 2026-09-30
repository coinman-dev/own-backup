//! What an encrypted image can say about itself **without anyone's key**.
//!
//! A backup folder has to be grouped into chains, shown in a list and swept by
//! retention on a machine that may hold no key at all — the service's key is
//! only opened while a job runs, and a boot medium has none until someone types
//! a password. So the open header carries the few fields those jobs need, and
//! this module is the one place that reads them.
//!
//! Everything here is a **claim**. It decides which file is whose parent and
//! how many versions a chain has held, not what any byte of data is. A caller
//! that holds the key must check these claims against the sealed sidecar
//! (`ObkImage::sealed_metadata`) before acting on anything that matters.

use std::path::Path;

use own_backup_common::Result;
use uuid::Uuid;

use super::bad;
use super::header::ObkFileHeader;
use crate::sidecar::ChainRole;

/// The open half of one encrypted image, in the terms `chains.rs` works in.
#[derive(Debug, Clone)]
pub struct ObkListing {
    /// Identity of the job whose key opens this image, from the portable
    /// envelope every image carries.
    pub job_id: String,
    pub chain_id: Uuid,
    pub backup_id: Uuid,
    pub role: ChainRole,
    pub created_at: String,
    /// File name of the parent image, from the qcow2 backing file field.
    pub parent_file_name: Option<String>,
    pub source_id: Option<String>,
    pub consolidated_increments: u32,
    pub chain_started_at: Option<String>,
    pub virtual_size: u64,
}

/// Read the open header of an `.obk`. Fails for anything that is not one.
pub fn read_listing(path: &Path) -> Result<ObkListing> {
    let file = std::fs::File::open(path)
        .map_err(|e| bad(format!("cannot open {}: {e}", path.display())))?;
    let mut reader = std::io::BufReader::new(file);
    let header =
        ObkFileHeader::read(&mut reader).map_err(|e| bad(format!("{}: {e}", path.display())))?;
    let open = header.open;
    Ok(ObkListing {
        job_id: open.key_file.job_id.clone(),
        chain_id: open.chain_id,
        backup_id: open.backup_id,
        role: open.role,
        created_at: open.created_at,
        // A parent is named by the file beside it; the identity it must have is
        // sealed in the root record and checked when the chain is opened.
        parent_file_name: header.backing_file.map(|name| {
            Path::new(&name)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or(name)
        }),
        source_id: open.source_id,
        consolidated_increments: open.consolidated_increments,
        chain_started_at: open.chain_started_at,
        virtual_size: open.virtual_size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seal::obk::container_tests::Fixture;
    use crate::seal::obk::{ObkWriter, ObkWriterOptions};

    fn write(path: &Path, f: &Fixture, opts: ObkWriterOptions) {
        let mut writer = ObkWriter::create(path, &f.chain, opts).unwrap();
        writer.write_cluster(0, &vec![7u8; 1 << 12]).unwrap();
        writer.finish(b"sealed sidecar").unwrap();
    }

    #[test]
    fn an_encrypted_image_describes_its_place_in_a_chain_without_a_key() {
        let f = Fixture::new();
        let dir = tempfile::tempdir().unwrap();
        let full_id = Uuid::new_v4();
        let mut opts = f.options(full_id, ChainRole::Full, None);
        opts.consolidated_increments = 3;
        opts.chain_started_at = Some("2026-09-01T00:00:00Z".into());
        write(&dir.path().join("full.obk"), &f, opts);

        let inc_id = Uuid::new_v4();
        write(
            &dir.path().join("inc.obk"),
            &f,
            f.options(inc_id, ChainRole::Incremental, Some((full_id, "full.obk"))),
        );

        let base = read_listing(&dir.path().join("full.obk")).unwrap();
        assert_eq!(base.job_id, "job-under-test");
        assert_eq!(base.role, ChainRole::Full);
        assert_eq!(base.backup_id, full_id);
        assert_eq!(base.parent_file_name, None);
        assert_eq!(base.consolidated_increments, 3);
        assert_eq!(
            base.chain_started_at.as_deref(),
            Some("2026-09-01T00:00:00Z")
        );
        assert_eq!(base.source_id.as_deref(), Some("volume-under-test"));

        let child = read_listing(&dir.path().join("inc.obk")).unwrap();
        assert_eq!(child.role, ChainRole::Incremental);
        assert_eq!(child.parent_file_name.as_deref(), Some("full.obk"));
        assert_eq!(child.chain_id, base.chain_id, "same chain");
        assert_ne!(child.backup_id, base.backup_id);
    }

    #[test]
    fn a_plaintext_image_is_not_mistaken_for_an_encrypted_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plain.qcow2");
        let mut writer = crate::qcow2::Qcow2StagedWriter::create(
            &path,
            crate::qcow2::Qcow2WriterOptions {
                virtual_size: 1 << 20,
                ..Default::default()
            },
        )
        .unwrap();
        writer.write_at(0, &[1u8; 4096]).unwrap();
        writer.finish().unwrap();
        assert!(read_listing(&path).is_err());
        assert!(read_listing(&dir.path().join("missing.obk")).is_err());
    }
}
