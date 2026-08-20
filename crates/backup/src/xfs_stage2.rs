//! XFS, этап D — чтение недостающих структур и «сухой» планировщик удаления.
//!
//! Stage 1 у xfs только зануляет данные; настоящее удаление (этап D плана
//! `.ai/xfs-true-removal-plan_2026-08-05.md`) обязано привести в согласованное
//! состояние семь классов структур сразу. Этот модуль — шаг D1: он ничего не
//! пишет, он только ЧИТАЕТ то, что раньше не читалось (AGI, INOBT, FINOBT,
//! RMAPBT, CNTBT), и говорит, что бы он изменил.
//!
//! Смысл «сухого» плана — сверка с эталоном. `scripts/xfs-delete-delta.py`
//! показывает, что при удалении файла делает САМО ЯДРО; тест
//! `plan_matches_the_kernel` требует, чтобы наш план совпал с этим по существу:
//! те же записи деревьев, те же дельты счётчиков. Тот же приём дал формулу
//! `h_checksum` в P8 — сначала посмотреть на эталон, потом писать код.
//!
//! Чего здесь намеренно нет: REFCBT (файлы с разделёнными reflink-экстентами мы
//! щадим целиком, так что дерево refcount'ов не трогается по построению) и
//! журнал (вход требует чистого лога, LSN в заголовках оставляем старыми).

use own_backup_common::{OwnBackupError, Result};

use crate::fat_map::RawVolume;
use crate::xfs_exclusions::{Extent, RemovableFile, Sb};
use std::collections::BTreeMap;

fn bad(msg: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::Device(format!("xfs stage2: {msg}"))
}

fn be16(b: &[u8], o: usize) -> u64 {
    u16::from_be_bytes([b[o], b[o + 1]]) as u64
}
fn be32(b: &[u8], o: usize) -> u64 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) as u64
}
fn be64(b: &[u8], o: usize) -> u64 {
    u64::from_be_bytes(b[o..o + 8].try_into().unwrap())
}

/// Заголовок B+дерева короткой формы (в пределах AG), v5.
const BTREE_HDR_V5: usize = 56;

pub const MAGIC_BNO: &[u8; 4] = b"AB3B";
pub const MAGIC_CNT: &[u8; 4] = b"AB3C";
pub const MAGIC_INO: &[u8; 4] = b"IAB3";
pub const MAGIC_FINO: &[u8; 4] = b"FIB3";
pub const MAGIC_RMAP: &[u8; 4] = b"RMB3";

/// Форма дерева: (длина записи, длина ключа, перекрывающееся ли).
///
/// У перекрывающегося дерева (только rmapbt) во внутреннем узле ДВА ключа на
/// слот — низкий и высокий; посчитать указатели по одному ключу значит уехать
/// в чужие байты (первый же обход упирался в суперблок).
fn shape(magic: &[u8; 4]) -> (usize, usize, bool) {
    match magic {
        MAGIC_BNO | MAGIC_CNT => (8, 8, false),
        MAGIC_INO | MAGIC_FINO => (16, 4, false),
        MAGIC_RMAP => (24, 20, true),
        _ => (0, 0, false),
    }
}

/// AGF — заголовок свободного места группы.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Agf {
    pub freeblks: u64,
    pub longest: u64,
    pub btreeblks: u64,
    pub flcount: u64,
    pub bno_root: u64,
    pub bno_level: u64,
    pub cnt_root: u64,
    pub cnt_level: u64,
    pub rmap_root: u64,
    pub rmap_level: u64,
}

/// AGI — заголовок инодов группы.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Agi {
    pub count: u64,
    pub freecount: u64,
    pub root: u64,
    pub level: u64,
    pub newino: u64,
    pub free_root: u64,
    pub free_level: u64,
}

/// Запись дерева свободного места (обе формы, BNO и CNT).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FreeExt {
    pub agbno: u64,
    pub len: u64,
}

/// Запись дерева инодов (INOBT/FINOBT).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct InoRec {
    pub startino: u64,
    pub holemask: u64,
    pub count: u64,
    pub freecount: u64,
    /// Битовая маска: 1 = инод свободен.
    pub free: u64,
}

/// Запись дерева обратных ссылок.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RmapRec {
    pub agbno: u64,
    pub len: u64,
    pub owner: u64,
    pub offset: u64,
}

pub(crate) struct AgReader<'a> {
    pub(crate) sb: &'a Sb,
    pub(crate) ag: u64,
}

impl<'a> AgReader<'a> {
    pub(crate) fn new(sb: &'a Sb, ag: u64) -> Self {
        Self { sb, ag }
    }

    fn ag_off(&self) -> u64 {
        self.ag * self.sb.agblocks * self.sb.blocksize
    }

    fn read_sector(&self, vol: &mut dyn RawVolume, sector: u64) -> Result<Vec<u8>> {
        let mut b = vec![0u8; self.sb.sectsize as usize];
        vol.read_at(self.ag_off() + sector * self.sb.sectsize, &mut b)?;
        Ok(b)
    }

    fn read_block(&self, vol: &mut dyn RawVolume, agbno: u64) -> Result<Vec<u8>> {
        if agbno >= self.sb.agblocks {
            return Err(bad(format!("AG {} блок {agbno} вне группы", self.ag)));
        }
        let mut b = vec![0u8; self.sb.blocksize as usize];
        vol.read_at(self.ag_off() + agbno * self.sb.blocksize, &mut b)?;
        Ok(b)
    }

    /// Физическое смещение сектора группы — чтобы D3 мог не только прочитать
    /// AGI, но и сказать, куда лечь правке.
    pub(crate) fn sector_offset(&self, sector: u64) -> u64 {
        self.ag_off() + sector * self.sb.sectsize
    }

    /// То же для блока.
    pub(crate) fn block_offset(&self, agbno: u64) -> u64 {
        self.ag_off() + agbno * self.sb.blocksize
    }

    pub(crate) fn read_sector_at(&self, vol: &mut dyn RawVolume, sector: u64) -> Result<Vec<u8>> {
        self.read_sector(vol, sector)
    }

    pub(crate) fn read_block_at(&self, vol: &mut dyn RawVolume, agbno: u64) -> Result<Vec<u8>> {
        self.read_block(vol, agbno)
    }

    pub(crate) fn agf(&self, vol: &mut dyn RawVolume) -> Result<Agf> {
        let b = self.read_sector(vol, 1)?;
        if &b[0..4] != b"XAGF" {
            return Err(bad(format!("AG {}: нет магии XAGF", self.ag)));
        }
        if be32(&b, 0x08) != self.ag {
            return Err(bad(format!("AG {}: agf_seqno не совпадает", self.ag)));
        }
        Ok(Agf {
            bno_root: be32(&b, 0x10),
            cnt_root: be32(&b, 0x14),
            rmap_root: be32(&b, 0x18),
            bno_level: be32(&b, 0x1C),
            cnt_level: be32(&b, 0x20),
            rmap_level: be32(&b, 0x24),
            flcount: be32(&b, 0x30),
            freeblks: be32(&b, 0x34),
            longest: be32(&b, 0x38),
            btreeblks: be32(&b, 0x3C),
        })
    }

    pub(crate) fn agi(&self, vol: &mut dyn RawVolume) -> Result<Agi> {
        let b = self.read_sector(vol, 2)?;
        if &b[0..4] != b"XAGI" {
            return Err(bad(format!("AG {}: нет магии XAGI", self.ag)));
        }
        if be32(&b, 0x08) != self.ag {
            return Err(bad(format!("AG {}: agi_seqno не совпадает", self.ag)));
        }
        Ok(Agi {
            count: be32(&b, 0x10),
            root: be32(&b, 0x14),
            level: be32(&b, 0x18),
            freecount: be32(&b, 0x1C),
            newino: be32(&b, 0x20),
            free_root: be32(&b, 0x148),
            free_level: be32(&b, 0x14C),
        })
    }

    /// Обойти дерево короткой формы, собрав записи листьев.
    fn walk<T>(
        &self,
        vol: &mut dyn RawVolume,
        root: u64,
        magic: &[u8; 4],
        parse: &dyn Fn(&[u8], usize) -> T,
        depth: u32,
        out: &mut Vec<T>,
    ) -> Result<()> {
        if depth > 16 {
            return Err(bad("дерево глубже 16 уровней — зациклилось"));
        }
        let (reclen, keylen, overlapped) = shape(magic);
        let blk = self.read_block(vol, root)?;
        if &blk[0..4] != magic {
            return Err(bad(format!(
                "AG {} блок {root}: магия {:?} вместо {:?}",
                self.ag,
                &blk[0..4],
                magic
            )));
        }
        let level = be16(&blk, 0x04);
        let nrecs = be16(&blk, 0x06) as usize;
        let bs = self.sb.blocksize as usize;
        if level == 0 {
            if BTREE_HDR_V5 + nrecs * reclen > bs {
                return Err(bad("лист заявляет больше записей, чем влезает"));
            }
            for i in 0..nrecs {
                out.push(parse(&blk, BTREE_HDR_V5 + i * reclen));
            }
            return Ok(());
        }
        let slot = keylen * if overlapped { 2 } else { 1 };
        let maxrecs = (bs - BTREE_HDR_V5) / (slot + 4);
        if nrecs > maxrecs {
            return Err(bad("узел заявляет больше указателей, чем влезает"));
        }
        let ptr_off = BTREE_HDR_V5 + maxrecs * slot;
        let children: Vec<u64> = (0..nrecs).map(|i| be32(&blk, ptr_off + i * 4)).collect();
        for c in children {
            self.walk(vol, c, magic, parse, depth + 1, out)?;
        }
        Ok(())
    }

    pub(crate) fn free_extents(&self, vol: &mut dyn RawVolume, by_size: bool) -> Result<Vec<FreeExt>> {
        let agf = self.agf(vol)?;
        let (root, magic) = if by_size {
            (agf.cnt_root, MAGIC_CNT)
        } else {
            (agf.bno_root, MAGIC_BNO)
        };
        let mut out = Vec::new();
        self.walk(
            vol,
            root,
            magic,
            &|b, o| FreeExt { agbno: be32(b, o), len: be32(b, o + 4) },
            0,
            &mut out,
        )?;
        Ok(out)
    }

    pub(crate) fn inode_records(&self, vol: &mut dyn RawVolume, free_tree: bool) -> Result<Vec<InoRec>> {
        let agi = self.agi(vol)?;
        let (root, magic) = if free_tree {
            (agi.free_root, MAGIC_FINO)
        } else {
            (agi.root, MAGIC_INO)
        };
        if root == 0 {
            return Ok(Vec::new());
        }
        let sparse = self.sb.spinodes;
        let mut out = Vec::new();
        self.walk(
            vol,
            root,
            magic,
            &move |b, o| {
                let (holemask, count, freecount) = if sparse {
                    (be16(b, o + 4), b[o + 6] as u64, b[o + 7] as u64)
                } else {
                    (0, 64, be32(b, o + 4))
                };
                InoRec { startino: be32(b, o), holemask, count, freecount, free: be64(b, o + 8) }
            },
            0,
            &mut out,
        )?;
        Ok(out)
    }

    pub(crate) fn rmap_records(&self, vol: &mut dyn RawVolume) -> Result<Vec<RmapRec>> {
        let agf = self.agf(vol)?;
        if agf.rmap_root == 0 {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        self.walk(
            vol,
            agf.rmap_root,
            MAGIC_RMAP,
            &|b, o| RmapRec {
                agbno: be32(b, o),
                len: be32(b, o + 4),
                owner: be64(b, o + 8),
                offset: be64(b, o + 16),
            },
            0,
            &mut out,
        )?;
        Ok(out)
    }
}

// --- «сухой» план ----------------------------------------------------------

/// Почему файл (или его часть) не удаляется по-настоящему. Каждая причина —
/// это либо будущий шаг плана, либо честный откат к Stage 1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Освобождается ПОСЛЕДНИЙ занятый инод чанка: ядро тогда убирает запись
    /// INOBT целиком и возвращает блоки чанка. Отдельный случай (шаг D3).
    LastInodeOfChunk { ino: u64 },
    /// Возврат экстента не примыкает ни к одной свободной записи — нужна
    /// вставка в B+дерево с возможным расщеплением узла (шаг D4).
    ExtentNeedsInsert { ag: u64, agbno: u64, len: u64 },
    /// Инод не нашёлся в дереве инодов — структура не та, что мы думаем.
    InodeNotInTree { ino: u64 },
}

/// Как вернётся один экстент.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExtentFate {
    /// Сливается с соседней свободной записью (обновление на месте).
    Merge { ag: u64, into: FreeExt, result: FreeExt },
    /// Требует вставки новой записи.
    Insert { ag: u64, rec: FreeExt },
}

/// Что мы БЫ сделали. Ни одного байта не пишется.
#[derive(Clone, Debug, Default)]
pub struct RemovalPlan {
    /// Иноды, которые освобождаются: (ino, ag, startino записи, номер бита).
    pub freed_inodes: Vec<(u64, u64, u64, u32)>,
    pub extents: Vec<ExtentFate>,
    /// Записи rmap, которые исчезнут (владелец — освобождаемый инод).
    pub rmap_deletes: Vec<(u64, RmapRec)>,
    /// Дельты счётчиков по группам и в суперблоке.
    pub agf_freeblks: BTreeMap<u64, i64>,
    pub agi_freecount: BTreeMap<u64, i64>,
    pub sb_fdblocks: i64,
    pub sb_ifree: i64,
    pub refusals: Vec<Refusal>,
}

/// Разложить номер инода на (AG, agino).
fn split_ino(sb: &Sb, ino: u64) -> (u64, u64) {
    let bits = sb.agblklog + sb.inopblog;
    (ino >> bits, ino & ((1u64 << bits) - 1))
}

/// Разложить номер fs-блока на (AG, agbno).
fn split_fsb(sb: &Sb, fsb: u64) -> (u64, u64) {
    (fsb >> sb.agblklog, fsb & ((1u64 << sb.agblklog) - 1))
}

/// Построить план удаления для файлов, которые Stage 1 уже признал своими.
///
/// Ничего не пишет и ни на что не жалуется в лог: это разведка, её результат
/// сверяется с эталонной дельтой ядра.
pub(crate) fn plan_removal(
    vol: &mut dyn RawVolume,
    sb: &Sb,
    files: &[RemovableFile],
) -> Result<RemovalPlan> {
    let mut plan = RemovalPlan::default();

    // Сколько инодов уходит из каждой записи INOBT — нужно, чтобы отличить
    // «освободили один из многих» от «освободили последний в чанке».
    let mut per_chunk: BTreeMap<(u64, u64), Vec<(u64, u32)>> = BTreeMap::new();
    let mut inobt_cache: BTreeMap<u64, Vec<InoRec>> = BTreeMap::new();
    for f in files {
        let (ag, agino) = split_ino(sb, f.ino);
        let recs = match inobt_cache.get(&ag) {
            Some(r) => r,
            None => {
                let r = AgReader::new(sb, ag).inode_records(vol, false)?;
                inobt_cache.entry(ag).or_insert(r)
            }
        };
        match recs.iter().find(|r| agino >= r.startino && agino < r.startino + 64) {
            Some(r) => {
                per_chunk.entry((ag, r.startino)).or_default().push((f.ino, (agino - r.startino) as u32));
            }
            None => plan.refusals.push(Refusal::InodeNotInTree { ino: f.ino }),
        }
    }
    for ((ag, startino), inos) in &per_chunk {
        let recs = &inobt_cache[ag];
        let rec = recs.iter().find(|r| r.startino == *startino).unwrap();
        // Занятых инодов в чанке: count минус свободные.
        let used = rec.count - rec.freecount;
        if used as usize == inos.len() {
            for (ino, _) in inos {
                plan.refusals.push(Refusal::LastInodeOfChunk { ino: *ino });
            }
            continue;
        }
        for (ino, bit) in inos {
            plan.freed_inodes.push((*ino, *ag, *startino, *bit));
            *plan.agi_freecount.entry(*ag).or_default() += 1;
            plan.sb_ifree += 1;
        }
    }

    // Экстенты освобождаемых файлов. Сначала СЛИВАЕМ соседние между собой:
    // ядро возвращает подряд идущие куски одной записью, и план, посчитанный
    // поштучно, разошёлся бы с эталоном на ровном месте.
    let freed: std::collections::HashSet<u64> =
        plan.freed_inodes.iter().map(|(ino, ..)| *ino).collect();
    let mut by_ag: BTreeMap<u64, Vec<(u64, u64)>> = BTreeMap::new(); // ag → (agbno, len)
    for f in files {
        if !freed.contains(&f.ino) {
            continue; // инод не освобождается — его экстенты тоже остаются
        }
        for e in &f.extents {
            let (ag, agbno) = split_fsb(sb, e.startblock);
            by_ag.entry(ag).or_default().push((agbno, e.count));
        }
    }
    for (&ag, list) in &mut by_ag {
        list.sort_unstable();
        let mut runs: Vec<(u64, u64)> = Vec::new();
        for &(agbno, len) in list.iter() {
            match runs.last_mut() {
                Some(last) if last.0 + last.1 == agbno => last.1 += len,
                _ => runs.push((agbno, len)),
            }
        }
        let free_list = AgReader::new(sb, ag).free_extents(vol, false)?;
        for (agbno, len) in runs {
            let end = agbno + len;
            let left = free_list.iter().find(|r| r.agbno + r.len == agbno).copied();
            let right = free_list.iter().find(|r| r.agbno == end).copied();
            let fate = match (left, right) {
                (Some(l), Some(r)) => ExtentFate::Merge {
                    ag,
                    into: l,
                    result: FreeExt { agbno: l.agbno, len: l.len + len + r.len },
                },
                (Some(l), None) => ExtentFate::Merge {
                    ag,
                    into: l,
                    result: FreeExt { agbno: l.agbno, len: l.len + len },
                },
                (None, Some(r)) => ExtentFate::Merge {
                    ag,
                    into: r,
                    result: FreeExt { agbno, len: len + r.len },
                },
                (None, None) => {
                    plan.refusals.push(Refusal::ExtentNeedsInsert { ag, agbno, len });
                    ExtentFate::Insert { ag, rec: FreeExt { agbno, len } }
                }
            };
            plan.extents.push(fate);
            *plan.agf_freeblks.entry(ag).or_default() += len as i64;
            plan.sb_fdblocks += len as i64;
        }
    }

    // Записи обратных ссылок, которые исчезнут вместе с файлами.
    for f in files {
        if !freed.contains(&f.ino) {
            continue;
        }
        let ags: std::collections::BTreeSet<u64> =
            f.extents.iter().map(|e| split_fsb(sb, e.startblock).0).collect();
        for ag in ags {
            let rl = AgReader::new(sb, ag).rmap_records(vol)?;
            for r in rl.into_iter().filter(|r| r.owner == f.ino) {
                plan.rmap_deletes.push((ag, r));
            }
        }
    }
    Ok(plan)
}

/// Экстенты файла в виде (AG, agbno, len) — для тестов и отчётов.
pub(crate) fn extent_parts(sb: &Sb, e: &Extent) -> (u64, u64, u64) {
    let (ag, agbno) = split_fsb(sb, e.startblock);
    (ag, agbno, e.count)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::exclusion_corpus as corpus;
    use crate::xfs_exclusions::{parse_sb, scan_xfs_exclusions};
    use std::os::unix::fs::FileExt;
    use std::path::Path;

    struct FileVol {
        f: std::fs::File,
        len: u64,
    }
    impl RawVolume for FileVol {
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
            self.f.read_exact_at(buf, offset).map_err(|e| bad(format!("read: {e}")))
        }
        fn len(&self) -> u64 {
            self.len
        }
    }

    fn vol(img: &Path) -> FileVol {
        let f = std::fs::File::open(img).unwrap();
        let len = f.metadata().unwrap().len();
        FileVol { f, len }
    }

    /// Счётчики суперблока, которые меняет удаление.
    fn sb_counters(img: &Path) -> (i64, i64, i64) {
        let mut b = [0u8; 512];
        vol(img).read_at(0, &mut b).unwrap();
        (be64(&b, 0x80) as i64, be64(&b, 0x88) as i64, be64(&b, 0x90) as i64)
    }

    /// D1: наш «сухой» план обязан совпасть с тем, что реально сделало ЯДРО.
    ///
    /// Образ строится, копируется, в копии те же файлы удаляются штатным
    /// `unlink` — и дальше мы сверяем не «похоже», а по существу: те же биты в
    /// INOBT, те же записи свободного места, те же исчезнувшие записи rmap, те
    /// же дельты счётчиков. Пока это совпадение не доказано, писать в чужую ФС
    /// нечего (`.ai/xfs-true-removal-plan_2026-08-05.md`, шаг D1).
    ///
    /// Нужен root (loop mount):
    ///
    /// ```sh
    /// cargo test -p own-backup-backup --lib --no-run
    /// sudo <test-binary> --ignored plan_matches_the_kernel --nocapture
    /// ```
    #[test]
    #[ignore = "needs root: builds an xfs image and deletes files with the kernel"]
    fn plan_matches_the_kernel() {
        if !corpus::is_root() {
            eprintln!("not root — skipping");
            return;
        }
        for t in ["mkfs.xfs", "xfs_repair"] {
            if std::process::Command::new(t).arg("-V").output().is_err() {
                eprintln!("{t} unavailable — skipping");
                return;
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("a.img");
        let after = dir.path().join("b.img");
        std::fs::File::create(&img).unwrap().set_len(512 * 1024 * 1024).unwrap();
        assert!(std::process::Command::new("mkfs.xfs")
            .args(["-q", "-m", "reflink=1,crc=1"])
            .arg(&img)
            .status()
            .unwrap()
            .success());
        let mnt = dir.path().join("mnt");
        std::fs::create_dir_all(&mnt).unwrap();

        // Каталоги нарочно короткоформатные (мало имён): удаление файлов тогда
        // не освобождает блоков КАТАЛОГА, и дельта ядра — ровно то, что мы
        // планируем, без посторонних слагаемых.
        // Жертвы и хранимые лежат в ОДНОМ каталоге и пишутся вперемешку:
        // XFS селит файлы каталога рядом, поэтому освобождаемые экстенты
        // получаются с разрывами. Когда они шли подряд, план сливался в одну
        // запись и про слияние не доказывал ничего.
        let victims: Vec<String> = (0..6).map(|i| format!("c{i}.bin")).collect();
        {
            let _m = corpus::mounted(&img, &mnt);
            std::fs::create_dir_all(mnt.join("mix")).unwrap();
            for i in 0..6u32 {
                let len = if i == 0 { 4 * 1024 * 1024 } else { 256 * 1024 };
                std::fs::write(mnt.join(format!("mix/c{i}.bin")), corpus::pattern(500 + i, len))
                    .unwrap();
                let _ = std::process::Command::new("sync").status();
                std::fs::write(mnt.join(format!("mix/k{i}.bin")), corpus::pattern(i, 256 * 1024))
                    .unwrap();
                let _ = std::process::Command::new("sync").status();
            }
        }
        std::fs::copy(&img, &after).unwrap();
        {
            let _m = corpus::mounted(&after, &mnt);
            for v in &victims {
                std::fs::remove_file(mnt.join("mix").join(v)).unwrap();
            }
        }

        // Наш план — по ИСХОДНОМУ образу.
        let mut v = vol(&img);
        let sb = parse_sb(&mut v).unwrap();
        let scan = scan_xfs_exclusions(&mut v, &["c*.bin".to_string()], |_| Ok(())).unwrap();
        assert_eq!(
            scan.removable.len(),
            victims.len(),
            "сканер нашёл не те файлы: {} вместо {}",
            scan.removable.len(),
            victims.len()
        );
        let plan = plan_removal(&mut v, &sb, &scan.removable).unwrap();
        eprintln!(
            "план: инодов {}, экстентов {}, rmap {}, отказов {}",
            plan.freed_inodes.len(),
            plan.extents.len(),
            plan.rmap_deletes.len(),
            plan.refusals.len()
        );
        for r in &plan.refusals {
            eprintln!("  отказ: {r:?}");
        }
        assert_eq!(plan.freed_inodes.len(), victims.len(), "не все иноды попали в план");

        let mut vb = vol(&after);

        // 1. Иноды: в эталоне тот же бит стал свободным.
        for &(ino, ag, startino, bit) in &plan.freed_inodes {
            let recs = AgReader::new(&sb, ag).inode_records(&mut vb, false).unwrap();
            let rec = recs
                .iter()
                .find(|r| r.startino == startino)
                .unwrap_or_else(|| panic!("запись INOBT {startino} исчезла (инод {ino})"));
            assert!(
                rec.free & (1u64 << bit) != 0,
                "инод {ino} (бит {bit} записи {startino}) ядро не освободило"
            );
        }

        // 2. Счётчики групп.
        for (&ag, &delta) in &plan.agi_freecount {
            let a = AgReader::new(&sb, ag).agi(&mut v).unwrap();
            let b = AgReader::new(&sb, ag).agi(&mut vb).unwrap();
            assert_eq!(
                b.freecount as i64 - a.freecount as i64,
                delta,
                "AG {ag}: agi_freecount разошёлся с ядром"
            );
        }
        for (&ag, &delta) in &plan.agf_freeblks {
            let a = AgReader::new(&sb, ag).agf(&mut v).unwrap();
            let b = AgReader::new(&sb, ag).agf(&mut vb).unwrap();
            assert_eq!(
                b.freeblks as i64 - a.freeblks as i64,
                delta,
                "AG {ag}: agf_freeblks разошёлся с ядром"
            );
        }

        // 3. Счётчики суперблока.
        let (ica, ifa, fda) = sb_counters(&img);
        let (icb, ifb, fdb) = sb_counters(&after);
        assert_eq!(icb - ica, 0, "sb_icount не должен меняться, пока чанки целы");
        assert_eq!(ifb - ifa, plan.sb_ifree, "sb_ifree разошёлся с ядром");
        assert_eq!(fdb - fda, plan.sb_fdblocks, "sb_fdblocks разошёлся с ядром");

        // 4. Свободное место: каждая запланированная запись есть в эталоне.
        for fate in &plan.extents {
            let (ag, want) = match fate {
                ExtentFate::Merge { ag, result, .. } => (*ag, *result),
                ExtentFate::Insert { ag, rec } => (*ag, *rec),
            };
            let after_free = AgReader::new(&sb, ag).free_extents(&mut vb, false).unwrap();
            assert!(
                after_free.contains(&want),
                "AG {ag}: ядро не создало запись {want:?}; у него {:?}",
                after_free
                    .iter()
                    .filter(|r| r.agbno.abs_diff(want.agbno) < 4096)
                    .collect::<Vec<_>>()
            );
        }

        // 5. Обратные ссылки: запланированные записи были и исчезли.
        for (ag, rec) in &plan.rmap_deletes {
            let before = AgReader::new(&sb, *ag).rmap_records(&mut v).unwrap();
            let after_r = AgReader::new(&sb, *ag).rmap_records(&mut vb).unwrap();
            assert!(before.contains(rec), "AG {ag}: записи rmap {rec:?} не было и до удаления");
            assert!(!after_r.contains(rec), "AG {ag}: ядро оставило запись rmap {rec:?}");
        }
        assert!(!plan.rmap_deletes.is_empty(), "план не тронул ни одной записи rmap");
        assert!(
            plan.extents.len() > 1,
            "фикстура выродилась: все экстенты слились в один — слияние не проверено"
        );
    }
}
