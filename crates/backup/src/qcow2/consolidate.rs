//! Fold a qcow2 child into its backing file (retention consolidation), and
//! repoint a child at a different backing file.
//!
//! This is the qcow2 half of the sliding-window scheme: FULL + N increments,
//! and when the window overflows the oldest increment is merged into the base,
//! which then stands for that increment's point in time. VHDX gets the same
//! done by `MergeVirtualDisk`; qcow2 has no kernel merge, so it is here.
//!
//! **Nothing is re-encoded.** The child's clusters are already zstd streams at
//! the image's own level, so they are copied byte-for-byte into the base and
//! their L2 entries rebuilt around the new host offsets — the CPU cost is a
//! memcpy, not a recompression, and a merge cannot change what a restore
//! reads. Clusters the child does not have are left alone: the base keeps its
//! own host offsets, so the file grows by the increment's compressed size and
//! not a byte more (plus dead space, below).
//!
//! Transactional through [`Qcow2InPlaceMerge::commit`]: every grafted cluster
//! and all fresh L1/L2/refcount metadata are appended and `fsync`'d BEFORE the
//! header is repointed. A crash before that leaves the base exactly as it was
//! — still a valid image of its own older point in time, with the child still
//! on disk beside it, so the merge can simply be run again. That is what makes
//! the whole consolidation idempotent up to its last two steps.
//!
//! **Dead space.** Where the child overrides a cluster the base already had,
//! the base's old host cluster loses its last reference. The commit writes it
//! out as refcount 0, so the image stays *consistent* — `qemu-img check` says
//! "No errors were found", not "leaked cluster", because nothing claims the
//! cluster (measured, not assumed: `qemu_validates_a_consolidated_chain`).
//! qcow2 calls that free space and qemu would reuse it; our writers are
//! append-only and never do. So consolidating night after night grows the file
//! beyond its live content, and the fix is a periodic rebuild rather than a
//! cleverer merge. `bytes_orphaned` is how the caller knows when one is due.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use own_backup_common::Result;

use super::{
    bad, ClusterDescriptor, Qcow2InPlaceMerge, Qcow2Reader, HEADER_LENGTH_V3, HEXT_BACKING_FORMAT,
    HEXT_END, QCOW2_MAGIC,
};

/// What one child-into-base merge moved.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Qcow2MergeStats {
    /// Guest clusters the child overrode (grafted or zeroed).
    pub clusters: u64,
    /// Bytes of cluster payload copied into the base.
    pub bytes_copied: u64,
    /// Bytes of base clusters that the graft un-referenced — dead space now
    /// sitting in the file. Accumulate this across merges to decide when a
    /// rebuild is due.
    pub bytes_orphaned: u64,
}

/// Resolve a backing-file name the way the chain reader does: relative names
/// against the image's own directory.
fn resolve_backing(image: &Path, backing: &str) -> PathBuf {
    let p = Path::new(backing);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        image.parent().unwrap_or_else(|| Path::new("")).join(p)
    }
}

/// Same file on disk? Compared through `canonicalize` so `./a.qcow2` and an
/// absolute path to it are one file; falls back to a plain compare when the
/// path cannot be canonicalised.
fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Merge `child` into `parent`, which must be the child's backing file.
///
/// Afterwards `parent` alone reads exactly as the chain `parent ← child` read
/// before, and `child` is redundant. The caller repoints the NEXT child (see
/// [`set_backing_file`]) and only then deletes `child` — the merge stays
/// re-runnable for as long as the child file exists.
pub fn merge_child_into_parent(parent: &Path, child: &Path) -> Result<Qcow2MergeStats> {
    // Two handles on the child: the reader decodes L2 entries, the plain file
    // hands out payload bytes at the host offsets the reader reports.
    let mut desc = Qcow2Reader::open(
        File::open(child)
            .map_err(|e| bad(format!("cannot open child {}: {e}", child.display())))?,
    )?;
    let mut raw = File::open(child)?;
    let child_len = raw.seek(SeekFrom::End(0))?;

    match desc.backing_file() {
        None => {
            return Err(bad(format!(
                "{} has no backing file — it is not a child of anything",
                child.display()
            )))
        }
        Some(name) => {
            let resolved = resolve_backing(child, name);
            if !same_file(&resolved, parent) {
                return Err(bad(format!(
                    "{} backs onto {}, not onto {} — refusing to merge",
                    child.display(),
                    resolved.display(),
                    parent.display()
                )));
            }
        }
    }

    let mut merge = Qcow2InPlaceMerge::open(parent, None)?;

    if desc.cluster_size() != merge.cluster_size() {
        return Err(bad(format!(
            "cluster size differs: child {} vs base {}",
            desc.cluster_size(),
            merge.cluster_size()
        )));
    }
    if desc.virtual_size() != merge.virtual_size() {
        return Err(bad(format!(
            "virtual size differs: child {} vs base {}",
            desc.virtual_size(),
            merge.virtual_size()
        )));
    }
    // Compressed payloads are copied verbatim, so both images must agree on
    // what a compressed cluster IS. Disagreeing here is not a corner case to
    // paper over — it would produce an image that decodes to garbage.
    if desc.is_zstd() != merge.is_zstd() {
        return Err(bad(format!(
            "compression differs: child zstd={} vs base zstd={}",
            desc.is_zstd(),
            merge.is_zstd()
        )));
    }

    let cluster_size = merge.cluster_size();
    let mut stats = Qcow2MergeStats::default();
    let mut buf = vec![0u8; cluster_size as usize];

    for index in 0..desc.total_clusters() {
        let descriptor = desc.descriptor(index)?;
        if descriptor == ClusterDescriptor::Unallocated {
            // The child defers to the base here — nothing to do.
            continue;
        }

        // Whatever the base held at this guest cluster is about to lose its
        // last reference.
        stats.bytes_orphaned += match merge.descriptor(index)? {
            ClusterDescriptor::Standard { .. } => cluster_size,
            ClusterDescriptor::Compressed { length, .. } => length,
            ClusterDescriptor::Zero | ClusterDescriptor::Unallocated => 0,
        };

        match descriptor {
            ClusterDescriptor::Unallocated => unreachable!("filtered above"),
            ClusterDescriptor::Zero => merge.graft_zero(index)?,
            ClusterDescriptor::Standard { host_offset } => {
                raw.seek(SeekFrom::Start(host_offset))?;
                raw.read_exact(&mut buf)?;
                merge.graft_raw(index, &buf)?;
                stats.bytes_copied += cluster_size;
            }
            ClusterDescriptor::Compressed {
                host_offset,
                length,
            } => {
                // `length` is rounded up to the end of the last 512-byte
                // sector the stream touches, so it can name bytes past the
                // frame — and, for the last cluster of the file, past EOF.
                // Copying the rounded run is correct either way: the decoder
                // stops at the frame end and ignores the tail (that is how the
                // format packs streams in the first place).
                let avail = child_len.saturating_sub(host_offset).min(length);
                if avail == 0 {
                    return Err(bad(format!(
                        "compressed cluster {index} points past the end of {}",
                        child.display()
                    )));
                }
                let mut payload = vec![0u8; avail as usize];
                raw.seek(SeekFrom::Start(host_offset))?;
                raw.read_exact(&mut payload)?;
                merge.graft_compressed(index, &payload)?;
                stats.bytes_copied += avail;
            }
        }
        stats.clusters += 1;
    }

    merge.commit()?;
    Ok(stats)
}

/// Point `image` at a different backing file (or none at all), rewriting its
/// header cluster in place.
///
/// The whole change — `backing_file_offset`, `backing_file_size`, the header
/// extensions and the name itself — lives in the first 512 bytes of the file,
/// one sector, which a device writes atomically or not at all. The rest of the
/// cluster is rewritten byte-identical. So an interrupted call leaves either
/// the old link or the new one, never a half-parsed name.
pub fn set_backing_file(image: &Path, backing: Option<&str>) -> Result<()> {
    if let Some(name) = backing {
        if name.is_empty() || name.len() > 1023 {
            return Err(bad("backing file name empty or longer than 1023 bytes"));
        }
    }

    let mut file = OpenOptions::new().read(true).write(true).open(image)?;
    let mut head = [0u8; HEADER_LENGTH_V3 as usize];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut head)?;

    if u32::from_be_bytes(head[0..4].try_into().unwrap()) != QCOW2_MAGIC {
        return Err(bad("bad magic — not a qcow2 image"));
    }
    if u32::from_be_bytes(head[4..8].try_into().unwrap()) != 3 {
        return Err(bad("repointing a backing file requires a version-3 image"));
    }
    let cluster_bits = u32::from_be_bytes(head[20..24].try_into().unwrap());
    if !(9..=21).contains(&cluster_bits) {
        return Err(bad(format!("cluster_bits {cluster_bits} out of range")));
    }
    let cluster_size = 1usize << cluster_bits;
    let header_length = u32::from_be_bytes(head[100..104].try_into().unwrap()) as usize;
    if header_length < HEADER_LENGTH_V3 as usize || header_length >= cluster_size {
        return Err(bad(format!("header length {header_length} out of range")));
    }

    let mut old = vec![0u8; cluster_size];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut old)?;

    // Walk the extensions so every one we do not own survives verbatim.
    let mut extensions: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut at = header_length;
    loop {
        if at + 8 > cluster_size {
            return Err(bad("header extensions run past the header cluster"));
        }
        let kind = u32::from_be_bytes(old[at..at + 4].try_into().unwrap());
        let len = u32::from_be_bytes(old[at + 4..at + 8].try_into().unwrap()) as usize;
        if kind == HEXT_END {
            break;
        }
        let end = at + 8 + len;
        if end > cluster_size {
            return Err(bad("header extension runs past the header cluster"));
        }
        extensions.push((kind, old[at + 8..end].to_vec()));
        at += 8 + len.div_ceil(8) * 8;
    }

    // Our own backing-format extension is re-emitted from the new state; every
    // other extension keeps its bytes and relative order.
    let mut out: Vec<(u32, Vec<u8>)> = extensions
        .into_iter()
        .filter(|(kind, _)| *kind != HEXT_BACKING_FORMAT)
        .collect();
    if backing.is_some() {
        out.insert(0, (HEXT_BACKING_FORMAT, b"qcow2".to_vec()));
    }

    let mut buf = vec![0u8; cluster_size];
    buf[..header_length].copy_from_slice(&old[..header_length]);
    let mut at = header_length;
    for (kind, data) in &out {
        let advance = 8 + data.len().div_ceil(8) * 8;
        if at + advance + 8 > cluster_size {
            return Err(bad("header extensions do not fit the header cluster"));
        }
        buf[at..at + 4].copy_from_slice(&kind.to_be_bytes());
        buf[at + 4..at + 8].copy_from_slice(&(data.len() as u32).to_be_bytes());
        buf[at + 8..at + 8 + data.len()].copy_from_slice(data);
        at += advance;
    }
    buf[at..at + 4].copy_from_slice(&HEXT_END.to_be_bytes());
    at += 8;

    let (name_offset, name_size) = match backing {
        Some(name) => {
            if at + name.len() >= cluster_size {
                return Err(bad("backing file name does not fit the header cluster"));
            }
            buf[at..at + name.len()].copy_from_slice(name.as_bytes());
            (at as u64, name.len() as u32)
        }
        None => (0u64, 0u32),
    };
    buf[8..16].copy_from_slice(&name_offset.to_be_bytes());
    buf[16..20].copy_from_slice(&name_size.to_be_bytes());

    file.seek(SeekFrom::Start(0))?;
    file.write_all(&buf)?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qcow2::{Qcow2RandomReader, Qcow2Writer, Qcow2WriterOptions};

    const CS: u64 = 65536;

    /// Build an image with the given clusters; `backing` makes it a child.
    fn write_image(path: &Path, vsize: u64, backing: Option<&str>, clusters: &[(u64, u8)]) {
        let mut w = Qcow2Writer::new(
            std::io::Cursor::new(Vec::new()),
            Qcow2WriterOptions {
                virtual_size: vsize,
                backing_file: backing.map(|s| s.to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        for &(index, fill) in clusters {
            w.write_cluster(index, &vec![fill; CS as usize]).unwrap();
        }
        std::fs::write(path, w.finish().unwrap().into_inner()).unwrap();
    }

    /// Read the whole virtual disk through the chain.
    fn read_all(path: &Path) -> Vec<u8> {
        let mut r = Qcow2RandomReader::open(path).unwrap();
        let mut buf = vec![0u8; r.virtual_size() as usize];
        r.read_at(0, &mut buf).unwrap();
        buf
    }

    /// The point of the whole exercise: after the merge the base alone must
    /// read byte-for-byte what the two-image chain read.
    #[test]
    fn merged_base_reads_like_the_chain() {
        let dir = tempfile::tempdir().unwrap();
        let vsize = 8 * CS;
        let base = dir.path().join("base.qcow2");
        let child = dir.path().join("child.qcow2");

        write_image(&base, vsize, None, &[(0, 1), (1, 2), (2, 3), (3, 4)]);
        // Child overrides cluster 1, adds 5, and blanks 2.
        write_image(&child, vsize, Some("base.qcow2"), &[(1, 0x20), (5, 0x50)]);
        {
            let mut m = Qcow2InPlaceMerge::open(&child, Some(3)).unwrap();
            m.write_cluster(2, &vec![0u8; CS as usize]).unwrap();
            m.commit().unwrap();
        }

        let before = read_all(&child);
        let stats = merge_child_into_parent(&base, &child).unwrap();
        assert_eq!(stats.clusters, 3, "clusters 1, 2 and 5 came from the child");

        let after = read_all(&base);
        assert_eq!(after, before, "merged base must read like the chain did");
        // And specifically: the blanked cluster is zeros, not the base's old 3.
        assert!(after[2 * CS as usize..3 * CS as usize]
            .iter()
            .all(|&b| b == 0));
    }

    /// A three-image chain consolidated one step at a time, exactly as the
    /// sliding window does it: merge the oldest child, repoint the next one.
    #[test]
    fn window_step_keeps_the_leaf_readable() {
        let dir = tempfile::tempdir().unwrap();
        let vsize = 8 * CS;
        let base = dir.path().join("base.qcow2");
        let inc1 = dir.path().join("inc1.qcow2");
        let inc2 = dir.path().join("inc2.qcow2");

        write_image(&base, vsize, None, &[(0, 1), (1, 1), (2, 1)]);
        write_image(&inc1, vsize, Some("base.qcow2"), &[(1, 2), (3, 2)]);
        write_image(&inc2, vsize, Some("inc1.qcow2"), &[(3, 3), (4, 3)]);

        let leaf_before = read_all(&inc2);

        merge_child_into_parent(&base, &inc1).unwrap();
        set_backing_file(&inc2, Some("base.qcow2")).unwrap();
        std::fs::remove_file(&inc1).unwrap();

        let leaf_after = read_all(&inc2);
        assert_eq!(
            leaf_after, leaf_before,
            "the leaf must survive consolidation"
        );
        // Cluster 1 now comes from the merged base (inc1's value), cluster 3
        // still from the leaf, which overrode inc1.
        assert_eq!(leaf_after[CS as usize], 2);
        assert_eq!(leaf_after[3 * CS as usize], 3);
    }

    /// Compressed clusters are copied, not re-encoded — so the merged base
    /// must not grow by a host cluster per grafted cluster.
    #[test]
    fn compressed_graft_packs_and_does_not_reencode() {
        let dir = tempfile::tempdir().unwrap();
        let vsize = 64 * CS;
        let base = dir.path().join("base.qcow2");
        let child = dir.path().join("child.qcow2");

        write_image(&base, vsize, None, &[(0, 1)]);
        // 32 highly compressible clusters — each shrinks to a few dozen bytes.
        let child_clusters: Vec<(u64, u8)> = (1..33).map(|c| (c, c as u8)).collect();
        write_image(&child, vsize, Some("base.qcow2"), &child_clusters);

        let base_before = std::fs::metadata(&base).unwrap().len();
        let stats = merge_child_into_parent(&base, &child).unwrap();
        let base_after = std::fs::metadata(&base).unwrap().len();

        assert_eq!(stats.clusters, 32);
        let growth = base_after - base_before;
        assert!(
            growth < 32 * CS,
            "32 compressible clusters grew the base by {growth} bytes — packing is not happening"
        );
        assert_eq!(
            read_all(&base)[CS as usize],
            1,
            "cluster 1 came from the child"
        );
    }

    /// Merging into the wrong file is the one mistake that silently destroys a
    /// backup, so it is refused before anything is written.
    #[test]
    fn refuses_a_child_that_backs_onto_something_else() {
        let dir = tempfile::tempdir().unwrap();
        let vsize = 4 * CS;
        let base = dir.path().join("base.qcow2");
        let other = dir.path().join("other.qcow2");
        let child = dir.path().join("child.qcow2");

        write_image(&base, vsize, None, &[(0, 1)]);
        write_image(&other, vsize, None, &[(0, 9)]);
        write_image(&child, vsize, Some("other.qcow2"), &[(1, 2)]);

        let before = std::fs::read(&base).unwrap();
        let err = merge_child_into_parent(&base, &child).unwrap_err();
        assert!(err.to_string().contains("refusing to merge"), "got: {err}");
        assert_eq!(
            std::fs::read(&base).unwrap(),
            before,
            "base must be untouched"
        );
    }

    /// Repointing must survive a name that is longer than the old one, and
    /// must be able to cut the link entirely.
    #[test]
    fn set_backing_file_handles_length_changes_and_removal() {
        let dir = tempfile::tempdir().unwrap();
        let vsize = 4 * CS;
        let base = dir.path().join("a.qcow2");
        let long = dir.path().join("a_much_longer_base_name.qcow2");
        let child = dir.path().join("child.qcow2");

        write_image(&base, vsize, None, &[(0, 1)]);
        write_image(&long, vsize, None, &[(0, 7)]);
        write_image(&child, vsize, Some("a.qcow2"), &[(1, 2)]);

        set_backing_file(&child, Some("a_much_longer_base_name.qcow2")).unwrap();
        {
            let r = Qcow2Reader::open(File::open(&child).unwrap()).unwrap();
            assert_eq!(r.backing_file(), Some("a_much_longer_base_name.qcow2"));
        }
        assert_eq!(
            read_all(&child)[0],
            7,
            "reads now fall through to the new base"
        );

        set_backing_file(&child, Some("a.qcow2")).unwrap();
        assert_eq!(read_all(&child)[0], 1, "and back again");

        set_backing_file(&child, None).unwrap();
        let r = Qcow2Reader::open(File::open(&child).unwrap()).unwrap();
        assert_eq!(r.backing_file(), None);
        assert_eq!(
            read_all(&child)[0],
            0,
            "no backing file → holes read as zeros"
        );
    }
}
