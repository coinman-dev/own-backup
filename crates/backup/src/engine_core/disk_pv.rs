//! A partition of a whole-disk copy that is an LVM physical volume in use.
//!
//! The session holds every active logical volume on it by a snapshot of its
//! own, all taken at one point in time (`own_backup_snapshot::pv`). The
//! partition is then assembled piece by piece, at its real offsets:
//!
//! * the parts of each logical volume — from that volume's snapshot, through
//!   its own used map and file exclusions, exactly as a volume copy of it
//!   would read them;
//! * free extents — not copied: LVM never reads them (the volume group's own
//!   metadata says which they are; when it cannot be read, nothing counts as
//!   free);
//! * everything else — the PV label, the metadata areas, the extents of
//!   inactive volumes — from the partition itself, which only LVM writes, and
//!   only when the volume group changes. Whether it did is checked before the
//!   image is published (`own_backup_snapshot::pv::check`).
//!
//! The image holds the partition sector for sector where it holds anything at
//! all: restored, the volume group comes back with every volume where it was.

use tracing::{info, warn};

use own_backup_common::lvm_pv;
use own_backup_common::{tr, OwnBackupError, Result};

use super::disk_prepare::prepare_partition_map;
use super::platform::{EnginePlatform, PhysicalVolume, SourceDevice};
use crate::exclusions::{ByteRanges, CategorySizes, ExcludedPath};
use crate::fat_map::RawVolume;
use crate::used_sectors::{AllUsedMap, UsedSectorMap};

/// The manifest's name for such a partition (as `blkid` calls it).
pub(crate) const PV_FILESYSTEM: &str = "LVM2_member";

/// Where one piece of the partition comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Src {
    /// The partition itself, at the same offset.
    Partition,
    /// Logical volume `index`, from `lv_offset` on.
    Volume { index: usize, lv_offset: u64 },
    /// A free extent: reads as zeros, never copied.
    Free,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Piece {
    start: u64,
    len: u64,
    from: Src,
}

/// A logical volume's reading: its snapshot, and what its exclusions zero
/// and patch (volume-relative).
struct VolumeRead {
    read_path: String,
    zero_ranges: ByteRanges,
    patches: Vec<(u64, Vec<u8>)>,
}

/// A prepared physical volume: how to read it and what its volumes' scans
/// found.
pub(crate) struct PvPrepared {
    /// Used blocks of the partition (partition-relative, in the copy's
    /// block size).
    pub(crate) used: Box<dyn UsedSectorMap>,
    pieces: Vec<Piece>,
    volumes: Vec<VolumeRead>,
    len: u64,
    /// NTFS names to delete inside the finished image: (offset in the
    /// partition, length of the volume, paths) — volumes in one piece only.
    pub(crate) removals: Vec<(u64, u64, Vec<ExcludedPath>)>,
    /// Why some excluded NTFS names stay in the copy.
    pub(crate) names_kept: Vec<String>,
    pub(crate) excluded_files: u64,
    pub(crate) excluded_bytes: u64,
    pub(crate) categories: Option<CategorySizes>,
    /// One line per volume whose files nobody walked.
    pub(crate) skips: Vec<String>,
    pub(crate) key_store_excluded: bool,
}

/// What the PV's own metadata says is free on it, as partition byte
/// ranges; `None` when it cannot be told — then nothing is free.
fn free_ranges<D: SourceDevice>(
    part_path: &str,
    part_len: u64,
    pv: &PhysicalVolume,
    source: &str,
) -> Result<Option<Vec<(u64, u64)>>> {
    let mut dev = D::open_read(part_path)?;
    let mut head = vec![0u8; lvm_pv::LABEL_SCAN_BYTES.min(part_len as usize)];
    dev.read_at(0, &mut head)?;
    let label = lvm_pv::parse_label(&head).ok_or_else(|| {
        OwnBackupError::NoSnapshotProvider(
            tr!("engine.pv_label_gone", volume = source).into_owned(),
        )
    })?;
    let mut why = String::from("no metadata area");
    for &(offset, size) in &label.metadata_areas {
        let size = size.min(lvm_pv::MAX_METADATA_AREA);
        if offset >= part_len || size == 0 {
            continue;
        }
        let mut area = vec![0u8; size.min(part_len - offset) as usize];
        dev.read_at(offset, &mut area)?;
        let Some(text) = lvm_pv::metadata_text(&area) else {
            why = format!("metadata area at {offset} unreadable");
            continue;
        };
        let alloc = match lvm_pv::allocation(&text, &label.uuid) {
            Ok(a) => a,
            Err(e) => {
                why = e;
                continue;
            }
        };
        if Some(alloc.pe_start) != label.pe_start() {
            why = "pe_start differs between the label and the metadata".into();
            continue;
        }
        // Every part of every active volume must be on extents the metadata
        // gives to a volume; otherwise it is not believed.
        let used = alloc.used_ranges();
        let inside = pv.volumes.iter().flat_map(|v| &v.segments).all(|s| {
            used.iter()
                .any(|&(a, b)| a <= s.pv_offset && s.pv_offset + s.length <= b)
        });
        if !inside {
            why = "an active volume lies on extents the metadata calls free".into();
            continue;
        }
        info!(
            volume = %source,
            vg = %alloc.vg,
            seqno = alloc.seqno,
            extent = alloc.extent_size,
            extents = alloc.pe_count,
            used = alloc.used.iter().map(|u| u.1).sum::<u64>(),
            other_pvs = alloc.other_pvs,
            "LVM physical volume: free extents are not copied"
        );
        if alloc.other_pvs {
            warn!(volume = %source, vg = %alloc.vg,
                  "the volume group has physical volumes on other disks: only this one is in the copy");
        }
        return Ok(Some(alloc.free_ranges()));
    }
    warn!(volume = %source, reason = %why,
          "LVM metadata not read — every extent outside the active volumes is copied");
    Ok(None)
}

/// Cut the partition into pieces: the volumes' parts, and between them free
/// extents and what is read from the partition itself.
fn pieces(
    part_len: u64,
    pv: &PhysicalVolume,
    free: Option<&[(u64, u64)]>,
    source: &str,
) -> Result<Vec<Piece>> {
    let mut parts: Vec<Piece> = pv
        .volumes
        .iter()
        .enumerate()
        .flat_map(|(index, v)| {
            v.segments.iter().map(move |s| Piece {
                start: s.pv_offset,
                len: s.length,
                from: Src::Volume {
                    index,
                    lv_offset: s.lv_offset,
                },
            })
        })
        .collect();
    parts.sort_by_key(|p| p.start);
    let bad = || OwnBackupError::Config(tr!("engine.pv_layout_bad", volume = source).into_owned());
    let mut out = Vec::new();
    let mut at = 0u64;
    let gap = |from: u64, to: u64, out: &mut Vec<Piece>| {
        let mut cur = from;
        for &(a, b) in free.unwrap_or(&[]) {
            let (a, b) = (a.max(from), b.min(to));
            if a >= b {
                continue;
            }
            if a > cur {
                out.push(Piece {
                    start: cur,
                    len: a - cur,
                    from: Src::Partition,
                });
            }
            out.push(Piece {
                start: a,
                len: b - a,
                from: Src::Free,
            });
            cur = b;
        }
        if cur < to {
            out.push(Piece {
                start: cur,
                len: to - cur,
                from: Src::Partition,
            });
        }
    };
    for p in parts {
        let end = p.start.checked_add(p.len).ok_or_else(bad)?;
        if p.start < at || end > part_len || p.len == 0 {
            return Err(bad());
        }
        gap(at, p.start, &mut out);
        out.push(p);
        at = end;
    }
    gap(at, part_len, &mut out);
    Ok(out)
}

/// Partition blocks in use, from the pieces and each volume's own map.
struct PieceMap {
    used: Vec<bool>,
    count: u64,
}

impl UsedSectorMap for PieceMap {
    fn is_block_used(&self, block_number: u64) -> bool {
        self.used
            .get(block_number as usize)
            .copied()
            .unwrap_or(false)
    }

    fn used_block_count(&self) -> u64 {
        self.count
    }
}

fn piece_map(
    part_len: u64,
    bs: u64,
    pieces: &[Piece],
    maps: &[Box<dyn UsedSectorMap>],
) -> PieceMap {
    let mut used = vec![false; part_len.div_ceil(bs) as usize];
    let mut mark = |from: u64, to: u64| {
        if from < to {
            for b in from / bs..=(to - 1) / bs {
                if let Some(slot) = used.get_mut(b as usize) {
                    *slot = true;
                }
            }
        }
    };
    for p in pieces {
        match p.from {
            Src::Free => {}
            Src::Partition => mark(p.start, p.start + p.len),
            Src::Volume { index, lv_offset } => {
                let map = &maps[index];
                let lv_end = lv_offset + p.len;
                for b in lv_offset / bs..=(lv_end - 1) / bs {
                    if map.is_block_used(b) {
                        let from = (b * bs).max(lv_offset);
                        let to = ((b + 1) * bs).min(lv_end);
                        mark(p.start + (from - lv_offset), p.start + (to - lv_offset));
                    }
                }
            }
        }
    }
    let count = used.iter().filter(|&&u| u).count() as u64;
    PieceMap { used, count }
}

/// Prepare the physical volume `pv` read through `part_path` (`part_len`
/// bytes): each volume's map and exclusions, the pieces, the partition's
/// used map. `source` and `part_index` name it in messages; `key_store`: the
/// session says the service's key directory is on it (decision 6/E4).
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_pv<P: EnginePlatform>(
    platform: &P,
    pv: &PhysicalVolume,
    part_path: &str,
    part_len: u64,
    part_index: u32,
    source: &str,
    block_size: u32,
    patterns_windows: &[String],
    patterns_linux: &[String],
    key_store: bool,
    mut tick: impl FnMut(u64, u64) -> Result<()>,
) -> Result<PvPrepared> {
    let bs = block_size as u64;
    let free = free_ranges::<P::Dev>(part_path, part_len, pv, source)?;
    let pieces = pieces(part_len, pv, free.as_deref(), source)?;

    if key_store && !pv.volumes.iter().any(|v| v.key_store) {
        return Err(super::volume_exclusions::key_store_refusal(
            source,
            &tr!("engine.key_store_pv_not_found"),
        ));
    }

    let mut out = PvPrepared {
        used: Box::new(AllUsedMap::new(0)),
        pieces: Vec::new(),
        volumes: Vec::new(),
        len: part_len,
        removals: Vec::new(),
        names_kept: Vec::new(),
        excluded_files: 0,
        excluded_bytes: 0,
        categories: None,
        skips: Vec::new(),
        key_store_excluded: false,
    };
    let mut maps: Vec<Box<dyn UsedSectorMap>> = Vec::new();
    for v in &pv.volumes {
        let with_key_store = |list: &[String]| -> Vec<String> {
            if v.key_store && list.is_empty() {
                vec![crate::seal::local::KEY_DIRECTORY_NAME.to_string()]
            } else {
                list.to_vec()
            }
        };
        let pm = prepare_partition_map(
            platform,
            &v.read_path,
            block_size,
            &None,
            &with_key_store(patterns_windows),
            &with_key_store(patterns_linux),
            v.frozen,
            P::METADATA_NEEDS_FROZEN && v.quiesced,
            &mut tick,
        )?;
        let shown = format!("{source} ({})", v.name);
        if v.key_store {
            if pm.map.is_none() || pm.categories.is_none() || pm.skipped.is_some() {
                let why = pm
                    .skipped
                    .clone()
                    .unwrap_or_else(|| tr!("engine.key_store_not_walked").into_owned());
                return Err(super::volume_exclusions::key_store_refusal(&shown, &why));
            }
            if let Some(l) = crate::exclusions::key_store_left_behind(&pm.left) {
                return Err(super::volume_exclusions::key_store_refusal(
                    &shown,
                    &tr!("engine.key_store_left_behind", path = l.shown()),
                ));
            }
            out.key_store_excluded = true;
            info!(volume = %shown, "the service key directory is left out of this copy");
        }
        let map = pm
            .map
            .unwrap_or_else(|| Box::new(AllUsedMap::new(v.len.div_ceil(bs))));
        info!(
            part = part_index,
            volume = %v.name,
            fs = %pm.filesystem,
            used_blocks = map.used_block_count(),
            excluded_files = pm.excluded_files,
            segments = v.segments.len(),
            "whole-disk backup: logical volume prepared"
        );
        out.excluded_files += pm.excluded_files;
        out.excluded_bytes += pm.excluded_bytes;
        if let Some(c) = &pm.categories {
            out.categories.get_or_insert_with(Default::default).merge(c);
        }
        if let Some(why) = &pm.skipped {
            out.skips.push(
                tr!(
                    "engine.skip_pv_volume_line",
                    index = part_index,
                    volume = v.name,
                    fs = pm.filesystem,
                    why = why
                )
                .into_owned(),
            );
        }
        if !pm.removal_paths.is_empty() {
            match v.segments.as_slice() {
                [s] if s.lv_offset == 0 && s.length == v.len => {
                    out.removals
                        .push((s.pv_offset, v.len, pm.removal_paths.clone()));
                }
                _ => out.names_kept.push(
                    tr!(
                        "engine.pv_names_kept",
                        volume = v.name,
                        count = pm.removal_paths.len()
                    )
                    .into_owned(),
                ),
            }
        }
        out.volumes.push(VolumeRead {
            read_path: v.read_path.clone(),
            zero_ranges: pm.zero_ranges,
            patches: pm.patches,
        });
        maps.push(map);
    }
    let map = piece_map(part_len, bs, &pieces, &maps);
    info!(
        part = part_index,
        volumes = pv.volumes.len(),
        pieces = pieces.len(),
        free_bytes = pieces
            .iter()
            .filter(|p| p.from == Src::Free)
            .map(|p| p.len)
            .sum::<u64>(),
        used_blocks = map.count,
        "whole-disk backup: LVM physical volume prepared"
    );
    out.used = Box::new(map);
    out.pieces = pieces;
    Ok(out)
}

impl PvPrepared {
    /// A reader of the whole partition as the copy takes it.
    pub(crate) fn open<D: SourceDevice>(&self, part_path: &str) -> Result<PvReader> {
        let mut volumes = Vec::with_capacity(self.volumes.len());
        for v in &self.volumes {
            volumes.push(PatchedVolume {
                inner: Box::new(D::open_read(&v.read_path)?),
                zero_ranges: v.zero_ranges.clone(),
                patches: v.patches.clone(),
            });
        }
        Ok(PvReader {
            part: Box::new(D::open_read(part_path)?),
            volumes,
            pieces: self.pieces.clone(),
            len: self.len,
        })
    }
}

/// A logical volume read with its exclusions applied.
struct PatchedVolume {
    inner: Box<dyn RawVolume + Send>,
    zero_ranges: ByteRanges,
    patches: Vec<(u64, Vec<u8>)>,
}

impl PatchedVolume {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.inner.read_at(offset, buf)?;
        self.zero_ranges.zero_overlaps(offset, buf);
        let end = offset + buf.len() as u64;
        for (at, bytes) in &self.patches {
            let (from, to) = (offset.max(*at), end.min(at + bytes.len() as u64));
            if from < to {
                buf[(from - offset) as usize..(to - offset) as usize]
                    .copy_from_slice(&bytes[(from - at) as usize..(to - at) as usize]);
            }
        }
        Ok(())
    }
}

/// The partition, assembled from its pieces.
pub(crate) struct PvReader {
    part: Box<dyn RawVolume + Send>,
    volumes: Vec<PatchedVolume>,
    pieces: Vec<Piece>,
    len: u64,
}

impl RawVolume for PvReader {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let end = offset + buf.len() as u64;
        if end > self.len {
            return Err(OwnBackupError::Device(format!(
                "read of {} bytes at {offset} past the end of the physical volume",
                buf.len()
            )));
        }
        let first = self.pieces.partition_point(|p| p.start + p.len <= offset);
        for p in &self.pieces[first..] {
            if p.start >= end {
                break;
            }
            let (from, to) = (offset.max(p.start), end.min(p.start + p.len));
            let dst = &mut buf[(from - offset) as usize..(to - offset) as usize];
            match p.from {
                Src::Free => dst.fill(0),
                Src::Partition => self.part.read_at(from, dst)?,
                Src::Volume { index, lv_offset } => {
                    self.volumes[index].read_at(lv_offset + (from - p.start), dst)?
                }
            }
        }
        Ok(())
    }

    fn len(&self) -> u64 {
        self.len
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine_core::platform::PvVolume;
    use own_backup_common::lvm_pv::PvSegment;

    fn volume(segments: &[(u64, u64, u64)]) -> PvVolume {
        PvVolume {
            name: "vg/lv".into(),
            read_path: String::new(),
            len: segments.iter().map(|s| s.1 + s.2).max().unwrap_or(0),
            segments: segments
                .iter()
                .map(|&(pv_offset, lv_offset, length)| PvSegment {
                    pv_offset,
                    lv_offset,
                    length,
                })
                .collect(),
            frozen: true,
            quiesced: true,
            key_store: false,
        }
    }

    #[test]
    fn the_partition_is_cut_at_every_volume_and_free_extent() {
        let pv = PhysicalVolume {
            volumes: vec![
                volume(&[(100, 0, 50), (300, 50, 50)]),
                volume(&[(150, 0, 20)]),
            ],
        };
        let free = [(170, 250), (350, 400)];
        let got = pieces(500, &pv, Some(&free), "/dev/x").unwrap();
        let want = [
            (0, 100, Src::Partition),
            (
                100,
                50,
                Src::Volume {
                    index: 0,
                    lv_offset: 0,
                },
            ),
            (
                150,
                20,
                Src::Volume {
                    index: 1,
                    lv_offset: 0,
                },
            ),
            (170, 80, Src::Free),
            (250, 50, Src::Partition),
            (
                300,
                50,
                Src::Volume {
                    index: 0,
                    lv_offset: 50,
                },
            ),
            (350, 50, Src::Free),
            (400, 100, Src::Partition),
        ];
        let got: Vec<_> = got.iter().map(|p| (p.start, p.len, p.from)).collect();
        assert_eq!(got, want);

        // Without the metadata nothing is free.
        let got = pieces(500, &pv, None, "/dev/x").unwrap();
        assert!(got.iter().all(|p| p.from != Src::Free));
        assert_eq!(got.iter().map(|p| p.len).sum::<u64>(), 500);

        // Overlapping parts, a part past the end: refused.
        let overlap = PhysicalVolume {
            volumes: vec![volume(&[(100, 0, 50)]), volume(&[(120, 0, 50)])],
        };
        assert!(pieces(500, &overlap, None, "/dev/x").is_err());
        let past = PhysicalVolume {
            volumes: vec![volume(&[(480, 0, 50)])],
        };
        assert!(pieces(500, &past, None, "/dev/x").is_err());
    }

    /// Only the used blocks of a volume count, where they lie on the
    /// partition; the partition's own pieces count whole; free ones not.
    #[test]
    fn the_partition_map_follows_each_volume_map() {
        struct Used(Vec<u64>);
        impl UsedSectorMap for Used {
            fn is_block_used(&self, b: u64) -> bool {
                self.0.contains(&b)
            }
            fn used_block_count(&self) -> u64 {
                self.0.len() as u64
            }
        }
        let bs = 10;
        let pieces = [
            Piece {
                start: 0,
                len: 15,
                from: Src::Partition,
            },
            // Volume blocks 1 and 3 used: LV bytes 10..20 and 30..40, at
            // partition 25..35 and 45..55 (the piece starts at 15 = LV 0).
            Piece {
                start: 15,
                len: 40,
                from: Src::Volume {
                    index: 0,
                    lv_offset: 0,
                },
            },
            Piece {
                start: 55,
                len: 45,
                from: Src::Free,
            },
        ];
        let maps: Vec<Box<dyn UsedSectorMap>> = vec![Box::new(Used(vec![1, 3]))];
        let map = piece_map(100, bs, &pieces, &maps);
        let used: Vec<u64> = (0..10).filter(|&b| map.is_block_used(b)).collect();
        assert_eq!(used, vec![0, 1, 2, 3, 4, 5]);
        assert_eq!(map.used_block_count(), 6);
    }
}
