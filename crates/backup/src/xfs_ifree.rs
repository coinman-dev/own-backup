//! XFS, шаг D3: освободить инод.
//!
//! Продолжение [`crate::xfs_dirent`] (D2, имя ушло из каталога). Здесь инод
//! перестаёт быть занятым: гасится его ядро, поднимается бит в дереве инодов,
//! правятся счётчики группы и суперблока. Блоки, которые он держал, остаются
//! размещёнными — их возвращает D4.
//!
//! **Сам по себе D3 тоже не даёт согласованной ФС.** После D2+D3 `xfs_repair`
//! уже не ругается на оторванный инод, зато находит блоки, которые никому не
//! принадлежат. Согласованность появляется только после D4, и собирает связку
//! [`crate::xfs_stage2::plan_stage2`] (D5).
//!
//! ## Что именно делает ядро
//!
//! Снято с живой ФС, не выведено из документации: `scripts/xfs-delete-delta.py`
//! показывает дельту деревьев и счётчиков, а раскладку ядра инода дала
//! отдельная проба. Ядро при удалении файла:
//!
//! - в самом иноде обнуляет `di_mode`, `di_nlink`, `di_size`, `di_nblocks`,
//!   `di_nextents` и увеличивает `di_gen` — поколение растёт, чтобы старые
//!   ссылки NFS не попали на переиспользованный инод;
//! - в записи INOBT поднимает бит свободы (номер бита = `agino - startino`,
//!   младшим вперёд) и увеличивает `freecount`;
//! - то же в FINOBT — **но записи там может не быть**: FINOBT хранит только
//!   чанки, где есть свободные иноды, и у полного чанка записи нет. Тогда её
//!   приходится ВСТАВЛЯТЬ. На эталонном прогоне из двух освобождённых инодов
//!   один потребовал именно вставки, так что это основной путь, а не редкость;
//! - увеличивает `agi_freecount` и `sb_ifree`.
//!
//! ## Где проходит граница
//!
//! Вставка делается только В ПРЕДЕЛАХ СУЩЕСТВУЮЩЕГО ЛИСТА: если лист полон и
//! потребовалось бы разбиение, инод щадится — он остаётся занятым, а файл
//! откатывается на Stage 1. Тот же приём («только в пределах листа плюс громкое
//! щажение») уже спас btrfs от трёх классов порчи. Разбиение узла — работа D4,
//! где оно нужно и для деревьев свободного места.
//!
//! При вставке в начало листа меняется его первый ключ, и ключ у родителя
//! обязан поехать следом — инвариант «ключ родителя равен первому ключу
//! потомка» проекту уже знаком: именно его нарушение в btrfs дало ФС, которую
//! `btrfs check` называл чистой, а ядро роняло с SIGBUS.

use std::collections::BTreeMap;

use own_backup_common::{OwnBackupError, Result};

use crate::fat_map::RawVolume;
use crate::xfs_dirent::{crc_matches, set_crc};
use crate::xfs_exclusions::Sb;
use crate::xfs_stage2::{AgReader, MAGIC_FINO, MAGIC_INO};

fn bad(msg: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::Device(format!("xfs ifree: {msg}"))
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
fn put_be16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_be_bytes());
}
fn put_be32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_be_bytes());
}
fn put_be64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_be_bytes());
}

/// Заголовок B+дерева короткой формы, v5.
const BTREE_HDR: usize = 56;
/// CRC блока дерева.
const BTREE_CRC_OFF: usize = 0x34;
/// Запись INOBT/FINOBT.
const INO_REC: usize = 16;
/// Ключ INOBT/FINOBT — `startino`.
const INO_KEY: usize = 4;
/// Инодов в чанке.
const INODES_PER_CHUNK: u64 = 64;

/// Поля AGI, которые трогает D3.
const AGI_FREECOUNT: usize = 0x1C;
const AGI_CRC_OFF: usize = 0x138;
/// Поля суперблока. `sb_icount` лежит по 0x80, а нужный нам `sb_ifree` —
/// СЛЕДОМ, по 0x88. Первая версия ошиблась на одно поле и прибавляла семь к
/// числу размещённых инодов; поймала это только приёмка на живой ФС —
/// `xfs_repair` сказал «sb_icount 519, counted 512» при том, что число инодов
/// обязано делиться на 64. Ни один единичный тест такого не увидел бы.
const SB_IFREE: usize = 0x88;
const SB_FDBLOCKS: usize = 0x90;
const SB_CRC_OFF: usize = 0xE0;
/// Поля ядра инода.
const DI_MODE: usize = 0x02;
const DI_NLINK: usize = 0x10;
const DI_SIZE: usize = 0x38;
const DI_NBLOCKS: usize = 0x40;
const DI_NEXTENTS: usize = 0x4C;
const DI_GEN: usize = 0x5C;
const DI_CRC_OFF: usize = 0x64;
const DI_CHANGECOUNT: usize = 0x68;

/// Почему инод остался занятым. Молчаливых пропусков в этом коде нет.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InoRefusal {
    /// Чанка с этим инодом в дереве нет.
    NotInTree { ino: u64 },
    /// Инод уже помечен свободным — освобождать нечего.
    AlreadyFree { ino: u64 },
    /// Вставка записи в FINOBT потребовала бы разбиения листа.
    FinobtLeafFull { ino: u64, ag: u64 },
    Other { ino: u64, why: String },
}

impl std::fmt::Display for InoRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InoRefusal::NotInTree { ino } => write!(f, "инод {ino}: чанка нет в дереве инодов"),
            InoRefusal::AlreadyFree { ino } => write!(f, "инод {ino}: уже свободен"),
            InoRefusal::FinobtLeafFull { ino, ag } => write!(
                f,
                "инод {ino}: лист FINOBT в группе {ag} полон, вставка потребовала бы разбиения"
            ),
            InoRefusal::Other { ino, why } => write!(f, "инод {ino}: {why}"),
        }
    }
}

/// Результат планирования.
#[derive(Debug, Default)]
pub(crate) struct InodeFreeing {
    pub(crate) patches: Vec<(u64, Vec<u8>)>,
    pub(crate) freed: Vec<u64>,
    pub(crate) refusals: Vec<InoRefusal>,
    /// Насколько вырос `sb_ifree`. Суперблок ЗДЕСЬ НЕ ПАТЧИТСЯ намеренно: D4
    /// правит в том же секторе `sb_fdblocks`, а патч — это целый сектор с
    /// пересчитанной контрольной суммой, так что два независимых патча просто
    /// затёрли бы друг друга. Оба шага возвращают дельты, и суперблок пишет
    /// [`patch_superblock`] один раз.
    pub(crate) sb_ifree_delta: u64,
}

/// Где лежит запись (или где ей место), вместе с дорогой от корня.
#[derive(Debug)]
struct Located {
    /// (agbno, номер слота) для каждого узла над листом.
    path: Vec<(u64, usize)>,
    leaf: u64,
    /// Индекс записи в листе — найденной или места вставки.
    idx: usize,
    found: bool,
}

/// Накопитель правок одной группы размещения: блоки правятся в памяти, чтобы
/// несколько инодов из одного листа не мешали друг другу.
struct AgEdits {
    ag: u64,
    blocks: BTreeMap<u64, Vec<u8>>,
}

impl AgEdits {
    fn block<'a>(
        &'a mut self,
        vol: &mut dyn RawVolume,
        sb: &Sb,
        agbno: u64,
        magic: &[u8; 4],
    ) -> Result<&'a mut Vec<u8>> {
        if !self.blocks.contains_key(&agbno) {
            let blk = AgReader::new(sb, self.ag).read_block_at(vol, agbno)?;
            if &blk[0..4] != magic {
                return Err(bad(format!(
                    "группа {} блок {agbno}: магия {:?} вместо {:?}",
                    self.ag,
                    &blk[0..4],
                    magic
                )));
            }
            if !crc_matches(&blk, BTREE_CRC_OFF) {
                return Err(bad(format!(
                    "группа {} блок {agbno}: CRC не сходится — не трогаю",
                    self.ag
                )));
            }
            self.blocks.insert(agbno, blk);
        }
        Ok(self.blocks.get_mut(&agbno).unwrap())
    }
}

/// Сколько записей влезает в лист и сколько ключей в узел.
fn leaf_maxrecs(sb: &Sb) -> usize {
    (sb.blocksize as usize - BTREE_HDR) / INO_REC
}
fn node_maxrecs(sb: &Sb) -> usize {
    (sb.blocksize as usize - BTREE_HDR) / (INO_KEY + 4)
}

/// Спуститься от корня к листу по ключу `key` (`startino`).
///
/// Правило спуска — как у ядра при поиске «меньше либо равно»: в узле берётся
/// последний ключ, не превышающий искомый. Для INOBT это даёт чанк, в котором
/// лежит инод; для FINOBT — либо саму запись, либо место, куда её вставлять.
fn descend(
    edits: &mut AgEdits,
    vol: &mut dyn RawVolume,
    sb: &Sb,
    root: u64,
    magic: &[u8; 4],
    key: u64,
) -> Result<Located> {
    let mut path = Vec::new();
    let mut agbno = root;
    for _ in 0..16 {
        let node_max = node_maxrecs(sb);
        let blk = edits.block(vol, sb, agbno, magic)?;
        let level = be16(blk, 0x04);
        let nrecs = be16(blk, 0x06) as usize;
        if level == 0 {
            if nrecs > leaf_maxrecs(sb) {
                return Err(bad("лист заявляет больше записей, чем влезает"));
            }
            let mut idx = nrecs;
            let mut found = false;
            for i in 0..nrecs {
                let k = be32(blk, BTREE_HDR + i * INO_REC);
                if k == key {
                    idx = i;
                    found = true;
                    break;
                }
                if k > key {
                    idx = i;
                    break;
                }
            }
            return Ok(Located { path, leaf: agbno, idx, found });
        }
        if nrecs == 0 || nrecs > node_max {
            return Err(bad("узел заявляет невозможное число указателей"));
        }
        let ptr_off = BTREE_HDR + node_max * INO_KEY;
        // Последний ключ, не превышающий искомый; если все больше — первый слот.
        let mut slot = 0usize;
        for i in 0..nrecs {
            if be32(blk, BTREE_HDR + i * INO_KEY) <= key {
                slot = i;
            } else {
                break;
            }
        }
        let child = be32(blk, ptr_off + slot * 4);
        path.push((agbno, slot));
        agbno = child;
    }
    Err(bad("дерево глубже 16 уровней — зациклилось"))
}

/// Прочитать запись INOBT/FINOBT из листа.
fn read_rec(blk: &[u8], idx: usize, sparse: bool) -> (u64, u64, u64, u64, u64) {
    let o = BTREE_HDR + idx * INO_REC;
    let startino = be32(blk, o);
    let (holemask, count, freecount) = if sparse {
        (be16(blk, o + 4), blk[o + 6] as u64, blk[o + 7] as u64)
    } else {
        (0, INODES_PER_CHUNK, be32(blk, o + 4))
    };
    (startino, holemask, count, freecount, be64(blk, o + 8))
}

/// Записать `freecount` и маску свободных обратно.
fn write_rec_free(blk: &mut [u8], idx: usize, sparse: bool, freecount: u64, free: u64) {
    let o = BTREE_HDR + idx * INO_REC;
    if sparse {
        blk[o + 7] = freecount as u8;
    } else {
        put_be32(blk, o + 4, freecount as u32);
    }
    put_be64(blk, o + 8, free);
}

/// Вставить 16-байтную запись в лист, если она туда влезает.
///
/// Возвращает `false`, когда лист полон: тогда инод щадится, а не выдавливается
/// разбиением узла.
fn insert_leaf_rec(blk: &mut [u8], sb: &Sb, idx: usize, rec: &[u8; INO_REC]) -> bool {
    let nrecs = be16(blk, 0x06) as usize;
    if nrecs + 1 > leaf_maxrecs(sb) {
        return false;
    }
    let base = BTREE_HDR + idx * INO_REC;
    let end = BTREE_HDR + nrecs * INO_REC;
    blk.copy_within(base..end, base + INO_REC);
    blk[base..base + INO_REC].copy_from_slice(rec);
    put_be16(blk, 0x06, (nrecs + 1) as u16);
    true
}

/// Поправить ключи родителей после того, как у листа сменился первый ключ.
///
/// Поднимаемся, пока правим НУЛЕВОЙ слот: если первый ключ узла тоже поехал,
/// его собственный родитель обязан узнать об этом.
fn fix_parent_keys(
    edits: &mut AgEdits,
    vol: &mut dyn RawVolume,
    sb: &Sb,
    magic: &[u8; 4],
    path: &[(u64, usize)],
    new_key: u64,
) -> Result<()> {
    for &(agbno, slot) in path.iter().rev() {
        let blk = edits.block(vol, sb, agbno, magic)?;
        put_be32(blk, BTREE_HDR + slot * INO_KEY, new_key as u32);
        if slot != 0 {
            break;
        }
    }
    Ok(())
}

/// Освободить перечисленные иноды.
///
/// Ничего не пишет: возвращает патчи и перечень щажений.
pub(crate) fn plan_inode_freeing(
    vol: &mut dyn RawVolume,
    sb: &Sb,
    inodes: &[u64],
) -> Result<InodeFreeing> {
    let mut out = InodeFreeing::default();
    if inodes.is_empty() {
        return Ok(out);
    }
    let sparse = sb.spinodes;
    let shift = sb.agblklog + sb.inopblog;

    // Разложить по группам: счётчики группы правятся один раз на все её иноды.
    let mut by_ag: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
    for &ino in inodes {
        by_ag.entry(ino >> shift).or_default().push(ino);
    }

    let mut sb_ifree_delta: u64 = 0;
    for (ag, inos) in by_ag {
        if ag >= sb.agcount {
            for ino in inos {
                out.refusals.push(InoRefusal::Other { ino, why: format!("группа {ag} не существует") });
            }
            continue;
        }
        let reader = AgReader::new(sb, ag);
        let agi = reader.agi(vol)?;
        if agi.root == 0 {
            for ino in inos {
                out.refusals.push(InoRefusal::NotInTree { ino });
            }
            continue;
        }
        let mut ino_edits = AgEdits { ag, blocks: BTreeMap::new() };
        let mut fino_edits = AgEdits { ag, blocks: BTreeMap::new() };
        let mut freed_here: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut agi_delta: u64 = 0;

        for ino in inos {
            let agino = ino & ((1u64 << shift) - 1);
            // 1. Чанк в INOBT.
            let loc = descend(&mut ino_edits, vol, sb, agi.root, MAGIC_INO, agino)?;
            let blk = ino_edits.block(vol, sb, loc.leaf, MAGIC_INO)?;
            let nrecs = be16(blk, 0x06) as usize;
            // Спуск «меньше либо равно» мог остановиться на следующей записи —
            // нужный чанк тогда предыдущий.
            let idx = if loc.found {
                loc.idx
            } else if loc.idx > 0 {
                loc.idx - 1
            } else {
                out.refusals.push(InoRefusal::NotInTree { ino });
                continue;
            };
            if idx >= nrecs {
                out.refusals.push(InoRefusal::NotInTree { ino });
                continue;
            }
            let (startino, holemask, count, freecount, free) = read_rec(blk, idx, sparse);
            if agino < startino || agino >= startino + INODES_PER_CHUNK {
                out.refusals.push(InoRefusal::NotInTree { ino });
                continue;
            }
            let bit = agino - startino;
            if free & (1u64 << bit) != 0 {
                out.refusals.push(InoRefusal::AlreadyFree { ino });
                continue;
            }
            if holemask & (1u64 << (bit / 4)) != 0 {
                out.refusals.push(InoRefusal::Other {
                    ino,
                    why: "инод попадает в дыру разреженного чанка".into(),
                });
                continue;
            }

            // 2. FINOBT: либо запись есть, либо её надо вставить. Решаем ДО
            //    правки INOBT — щажение не должно оставлять полуправку.
            let new_free = free | (1u64 << bit);
            let new_freecount = freecount + 1;
            let mut fino_plan: Option<(Located, bool)> = None;
            if agi.free_root != 0 {
                let floc = descend(&mut fino_edits, vol, sb, agi.free_root, MAGIC_FINO, startino)?;
                if !floc.found {
                    let fblk = fino_edits.block(vol, sb, floc.leaf, MAGIC_FINO)?;
                    let fnrecs = be16(fblk, 0x06) as usize;
                    if fnrecs + 1 > leaf_maxrecs(sb) {
                        out.refusals.push(InoRefusal::FinobtLeafFull { ino, ag });
                        continue;
                    }
                }
                fino_plan = Some((floc, true));
            }

            // 3. Правка INOBT.
            let blk = ino_edits.block(vol, sb, loc.leaf, MAGIC_INO)?;
            write_rec_free(blk, idx, sparse, new_freecount, new_free);

            // 4. Правка FINOBT.
            if let Some((floc, _)) = fino_plan {
                let fblk = fino_edits.block(vol, sb, floc.leaf, MAGIC_FINO)?;
                if floc.found {
                    write_rec_free(fblk, floc.idx, sparse, new_freecount, new_free);
                } else {
                    let mut rec = [0u8; INO_REC];
                    put_be32(&mut rec, 0, startino as u32);
                    if sparse {
                        put_be16(&mut rec, 4, holemask as u16);
                        rec[6] = count as u8;
                        rec[7] = new_freecount as u8;
                    } else {
                        put_be32(&mut rec, 4, new_freecount as u32);
                    }
                    put_be64(&mut rec, 8, new_free);
                    if !insert_leaf_rec(fblk, sb, floc.idx, &rec) {
                        out.refusals.push(InoRefusal::FinobtLeafFull { ino, ag });
                        // INOBT уже правлен — откатывать нечего: запись в INOBT
                        // без записи в FINOBT это порча, поэтому вернуть биты.
                        let blk = ino_edits.block(vol, sb, loc.leaf, MAGIC_INO)?;
                        write_rec_free(blk, idx, sparse, freecount, free);
                        continue;
                    }
                    if floc.idx == 0 {
                        fix_parent_keys(
                            &mut fino_edits,
                            vol,
                            sb,
                            MAGIC_FINO,
                            &floc.path,
                            startino,
                        )?;
                    }
                }
            }

            // 5. Ядро инода.
            let mut core = read_inode_raw(vol, sb, ino)?;
            if be16(&core, DI_MODE) == 0 {
                out.refusals.push(InoRefusal::AlreadyFree { ino });
                let blk = ino_edits.block(vol, sb, loc.leaf, MAGIC_INO)?;
                write_rec_free(blk, idx, sparse, freecount, free);
                continue;
            }
            put_be16(&mut core, DI_MODE, 0);
            put_be32(&mut core, DI_NLINK, 0);
            put_be64(&mut core, DI_SIZE, 0);
            put_be64(&mut core, DI_NBLOCKS, 0);
            put_be32(&mut core, DI_NEXTENTS, 0);
            let gen = be32(&core, DI_GEN) as u32;
            put_be32(&mut core, DI_GEN, gen.wrapping_add(1));
            let cc = be64(&core, DI_CHANGECOUNT);
            put_be64(&mut core, DI_CHANGECOUNT, cc.wrapping_add(1));
            set_crc(&mut core, DI_CRC_OFF);
            freed_here.push((sb.inode_offset(ino), core));

            agi_delta += 1;
            out.freed.push(ino);
        }

        if agi_delta == 0 {
            continue;
        }
        // Счётчик группы.
        let mut agi_sec = reader.read_sector_at(vol, 2)?;
        if !crc_matches(&agi_sec, AGI_CRC_OFF) {
            return Err(bad(format!("группа {ag}: CRC AGI не сходится — не трогаю")));
        }
        let fc = be32(&agi_sec, AGI_FREECOUNT);
        put_be32(&mut agi_sec, AGI_FREECOUNT, (fc + agi_delta) as u32);
        set_crc(&mut agi_sec, AGI_CRC_OFF);
        out.patches.push((reader.sector_offset(2), agi_sec));

        for (agbno, mut blk) in ino_edits.blocks {
            set_crc(&mut blk, BTREE_CRC_OFF);
            out.patches.push((reader.block_offset(agbno), blk));
        }
        for (agbno, mut blk) in fino_edits.blocks {
            set_crc(&mut blk, BTREE_CRC_OFF);
            out.patches.push((reader.block_offset(agbno), blk));
        }
        out.patches.extend(freed_here);
        sb_ifree_delta += agi_delta;
    }

    out.sb_ifree_delta = sb_ifree_delta;
    Ok(out)
}

/// Один патч суперблока на оба счётчика.
///
/// `sb_ifree` растёт от D3, `sb_fdblocks` — от D4, и живут они в одном секторе
/// под одной контрольной суммой. Поэтому патч ровно один, и делается он после
/// того, как оба шага сказали свои дельты.
pub(crate) fn patch_superblock(
    vol: &mut dyn RawVolume,
    sb: &Sb,
    ifree_delta: u64,
    fdblocks_delta: u64,
) -> Result<Option<(u64, Vec<u8>)>> {
    if ifree_delta == 0 && fdblocks_delta == 0 {
        return Ok(None);
    }
    let mut sec = vec![0u8; sb.sectsize as usize];
    vol.read_at(0, &mut sec)?;
    if !crc_matches(&sec, SB_CRC_OFF) {
        return Err(bad("CRC суперблока не сходится — не трогаю"));
    }
    let ifree = be64(&sec, SB_IFREE);
    put_be64(&mut sec, SB_IFREE, ifree + ifree_delta);
    let fd = be64(&sec, SB_FDBLOCKS);
    put_be64(&mut sec, SB_FDBLOCKS, fd + fdblocks_delta);
    set_crc(&mut sec, SB_CRC_OFF);
    Ok(Some((0, sec)))
}

fn read_inode_raw(vol: &mut dyn RawVolume, sb: &Sb, ino: u64) -> Result<Vec<u8>> {
    let off = sb.inode_offset(ino);
    let mut buf = vec![0u8; sb.inodesize as usize];
    vol.read_at(off, &mut buf)?;
    if &buf[0..2] != b"IN" {
        return Err(bad(format!("инод {ino}: нет магии IN")));
    }
    if !crc_matches(&buf, DI_CRC_OFF) {
        return Err(bad(format!("инод {ino}: CRC не сходится — не трогаю")));
    }
    Ok(buf)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn blank_leaf(bs: usize, nrecs: u16) -> Vec<u8> {
        let mut b = vec![0u8; bs];
        b[0..4].copy_from_slice(MAGIC_FINO);
        put_be16(&mut b, 0x04, 0); // level 0
        put_be16(&mut b, 0x06, nrecs);
        b
    }

    fn test_sb() -> Sb {
        Sb {
            blocksize: 4096,
            logstart: 0,
            logblocks: 0,
            inodesize: 512,
            agblocks: 1024,
            agcount: 4,
            agblklog: 10,
            inopblog: 3,
            rootino: 128,
            sectsize: 512,
            reflink: true,
            ftype: true,
            spinodes: true,
        }
    }

    /// Номер бита — `agino - startino`, младшим вперёд. Значение сверено с
    /// ядром: инод 133 в чанке со startino=128 поднял бит 5 (маска
    /// 0xffffffffffffff80 → 0xffffffffffffffa0).
    #[test]
    fn the_free_bit_matches_the_kernel() {
        let bit = 133u64 - 128;
        assert_eq!(bit, 5);
        let before = 0xffff_ffff_ffff_ff80u64;
        assert_eq!(before | (1u64 << bit), 0xffff_ffff_ffff_ffa0);
    }

    /// Вставка сдвигает соседей и растит `numrecs`, не трогая остального.
    #[test]
    fn inserting_shifts_the_neighbours() {
        let sb = test_sb();
        let mut blk = blank_leaf(4096, 3);
        for i in 0..3u32 {
            put_be32(&mut blk, BTREE_HDR + i as usize * INO_REC, 100 + i * 64);
        }
        let mut rec = [0u8; INO_REC];
        put_be32(&mut rec, 0, 150);
        assert!(insert_leaf_rec(&mut blk, &sb, 1, &rec));
        assert_eq!(be16(&blk, 0x06), 4);
        let keys: Vec<u64> =
            (0..4).map(|i| be32(&blk, BTREE_HDR + i * INO_REC)).collect();
        assert_eq!(keys, vec![100, 150, 164, 228]);
    }

    /// Полный лист не разбивается, а честно отказывает.
    #[test]
    fn a_full_leaf_refuses_instead_of_splitting() {
        let sb = test_sb();
        let max = leaf_maxrecs(&sb);
        let mut blk = blank_leaf(4096, max as u16);
        let rec = [0u8; INO_REC];
        assert!(!insert_leaf_rec(&mut blk, &sb, 0, &rec), "полный лист принял запись");
        assert_eq!(be16(&blk, 0x06), max as u64, "numrecs изменился при отказе");
    }

    /// Ключ родителя обязан равняться первому ключу потомка, и поправка
    /// поднимается вверх, пока правится нулевой слот. Нарушение этого
    /// инварианта в btrfs дало ФС, которую fsck называл чистой, а ядро роняло.
    #[test]
    fn parent_keys_follow_the_first_key_upwards() {
        let sb = test_sb();
        let mut edits = AgEdits { ag: 0, blocks: BTreeMap::new() };
        // Два уровня узлов над листом; лист — нулевой слот у обоих.
        for agbno in [10u64, 20] {
            let mut b = vec![0u8; 4096];
            b[0..4].copy_from_slice(MAGIC_FINO);
            put_be16(&mut b, 0x04, 1);
            put_be16(&mut b, 0x06, 2);
            put_be32(&mut b, BTREE_HDR, 500);
            put_be32(&mut b, BTREE_HDR + INO_KEY, 900);
            edits.blocks.insert(agbno, b);
        }
        struct NoVol;
        impl RawVolume for NoVol {
            fn read_at(&mut self, _o: u64, _b: &mut [u8]) -> Result<()> {
                panic!("блоки уже в памяти — чтения быть не должно")
            }
            fn len(&self) -> u64 {
                0
            }
        }
        fix_parent_keys(&mut edits, &mut NoVol, &sb, MAGIC_FINO, &[(20, 0), (10, 0)], 320).unwrap();
        assert_eq!(be32(&edits.blocks[&10], BTREE_HDR), 320, "родитель листа не поехал");
        assert_eq!(be32(&edits.blocks[&20], BTREE_HDR), 320, "дед не поехал");
        assert_eq!(be32(&edits.blocks[&10], BTREE_HDR + INO_KEY), 900, "сосед сдвинулся");

        // А если лист висит НЕ на нулевом слоте, выше подниматься нельзя.
        let mut edits2 = AgEdits { ag: 0, blocks: BTreeMap::new() };
        for agbno in [10u64, 20] {
            let mut b = vec![0u8; 4096];
            b[0..4].copy_from_slice(MAGIC_FINO);
            put_be16(&mut b, 0x04, 1);
            put_be16(&mut b, 0x06, 2);
            put_be32(&mut b, BTREE_HDR, 500);
            put_be32(&mut b, BTREE_HDR + INO_KEY, 900);
            edits2.blocks.insert(agbno, b);
        }
        fix_parent_keys(&mut edits2, &mut NoVol, &sb, MAGIC_FINO, &[(20, 0), (10, 1)], 800).unwrap();
        assert_eq!(be32(&edits2.blocks[&10], BTREE_HDR + INO_KEY), 800);
        assert_eq!(be32(&edits2.blocks[&20], BTREE_HDR), 500, "дед поехал зря");
    }

    /// Приёмка D2+D3 на живой ФС.
    ///
    /// После двух шагов оторванных инодов уже нет — их не стало вовсе. Что
    /// осталось, то и есть работа D4: блоки удалённых файлов всё ещё числятся
    /// занятыми, а RMAPBT помнит их за освобождённым инодом. Тест перечисляет
    /// ровно этот остаток и ловит всё остальное — если бы поехала маска
    /// свободных инодов, счётчик группы или ключ в FINOBT, жалоба выглядела бы
    /// иначе.
    ///
    /// Динамическая половина строже статики: ядро обязано смонтировать,
    /// прочитать уцелевшее и СОЗДАТЬ новые файлы — то есть воспользоваться
    /// освобождёнными инодами по-настоящему.
    #[test]
    #[ignore = "needs root: loop-mounts the patched image and writes to it"]
    fn d2_and_d3_free_the_inode_the_kernel_agrees() {
        use crate::exclusion_corpus as corpus;
        use crate::xfs_dirent::plan_dirent_removal;
        use crate::xfs_exclusions::parse_sb;
        use std::os::linux::fs::MetadataExt;
        use std::os::unix::fs::FileExt;

        if !corpus::is_root() {
            eprintln!("не root — пропускаю");
            return;
        }
        for t in ["mkfs.xfs", "xfs_repair"] {
            if std::process::Command::new(t).arg("-V").output().is_err() {
                eprintln!("нет {t} — пропускаю");
                return;
            }
        }
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

        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("fs.img");
        std::fs::File::create(&img).unwrap().set_len(768 * 1024 * 1024).unwrap();
        assert!(std::process::Command::new("mkfs.xfs")
            .args(["-q", "-m", "reflink=1,crc=1"])
            .arg(&img)
            .status()
            .unwrap()
            .success());
        let mnt = dir.path().join("mnt");
        std::fs::create_dir_all(&mnt).unwrap();

        // Каталог формы leaf (как в корпусе) плюс маленький shortform.
        let (dir_inos, victims, kept) = {
            let _m = corpus::mounted(&img, &mnt);
            let mut dir_inos = Vec::new();
            let mut victims: Vec<(u64, Vec<String>, Vec<u64>)> = Vec::new();
            let mut kept = Vec::new();
            for (name, n, vics) in [("small", 3u32, 1usize), ("dense", 400, 6)] {
                let d = mnt.join(name);
                std::fs::create_dir(&d).unwrap();
                for i in 0..n {
                    let f = d.join(format!("file-{i:05}.dat"));
                    std::fs::write(&f, b"k".repeat(2048)).unwrap();
                    kept.push((name.to_string(), format!("file-{i:05}.dat")));
                }
                let mut names = Vec::new();
                let mut inos = Vec::new();
                for k in 0..vics {
                    let f = d.join(format!("VICTIM-{k}-long-name.dat"));
                    // Разные размеры: и однокластерные, и многоблочные.
                    std::fs::write(&f, b"v".repeat(4096 * (k + 1) * 8)).unwrap();
                    inos.push(std::fs::metadata(&f).unwrap().st_ino());
                    names.push(format!("VICTIM-{k}-long-name.dat"));
                }
                let dino = std::fs::metadata(&d).unwrap().st_ino();
                dir_inos.push((name.to_string(), dino));
                victims.push((dino, names, inos));
            }
            (dir_inos, victims, kept)
        };

        // D2 + D3.
        let f = std::fs::File::open(&img).unwrap();
        let len = f.metadata().unwrap().len();
        let mut vol = FileVol { f, len };
        let sb = parse_sb(&mut vol).unwrap();
        let mut patches: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut all_inos = Vec::new();
        for (dino, names, inos) in &victims {
            let edit = plan_dirent_removal(&mut vol, &sb, *dino, names).unwrap();
            assert!(edit.refusals.is_empty(), "D2 отказал: {:?}", edit.refusals);
            assert_eq!(edit.removed.len(), names.len());
            patches.extend(edit.patches);
            all_inos.extend(inos.iter().copied());
        }
        let freeing = plan_inode_freeing(&mut vol, &sb, &all_inos).unwrap();
        assert!(
            freeing.refusals.is_empty(),
            "D3 пощадил иноды: {:?}",
            freeing.refusals.iter().map(|r| r.to_string()).collect::<Vec<_>>()
        );
        assert_eq!(freeing.freed.len(), all_inos.len(), "освобождены не все иноды");
        // Суперблок пишется отдельно — D3 отдаёт только дельту.
        if let Some(sbp) = patch_superblock(&mut vol, &sb, freeing.sb_ifree_delta, 0).unwrap() {
            patches.push(sbp);
        }
        eprintln!(
            "  каталоги: {:?}; освобождено инодов: {}; патчей: {}",
            dir_inos,
            freeing.freed.len(),
            patches.len() + freeing.patches.len()
        );
        patches.extend(freeing.patches);
        drop(vol);

        let w = std::fs::OpenOptions::new().write(true).open(&img).unwrap();
        for (off, bytes) in &patches {
            w.write_all_at(bytes, *off).unwrap();
        }
        w.sync_all().unwrap();
        drop(w);

        // Статика СРАЗУ после правки, до монтирования: иначе не отличить нашу
        // ошибку от того, что ядро сделало со счётчиками за сеанс записи.
        let pre = std::process::Command::new("xfs_repair").arg("-n").arg(&img).output().unwrap();
        let pre_text = format!(
            "{}{}",
            String::from_utf8_lossy(&pre.stdout),
            String::from_utf8_lossy(&pre.stderr)
        );
        eprintln!("─── xfs_repair -n ДО монтирования ───\n{pre_text}───────────");

        // Ядро.
        let before = corpus::dmesg_len();
        {
            let _m = corpus::mounted(&img, &mnt);
            for (dname, names, _) in &victims {
                let _ = dname;
                let _ = names;
            }
            for (dname, dino) in &dir_inos {
                let _ = dino;
                let d = mnt.join(dname);
                let listed: std::collections::HashSet<String> = std::fs::read_dir(&d)
                    .unwrap()
                    .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
                    .collect();
                assert!(
                    !listed.iter().any(|n| n.starts_with("VICTIM")),
                    "каталог {dname}: жертвы остались: {:?}",
                    listed.iter().filter(|n| n.starts_with("VICTIM")).collect::<Vec<_>>()
                );
            }
            for (dname, fname) in &kept {
                let body = std::fs::read(mnt.join(dname).join(fname)).unwrap();
                assert_eq!(body.len(), 2048, "сосед {dname}/{fname} прочитался иначе");
            }
            // Новые файлы: освобождённые иноды обязаны быть пригодны, а не
            // просто помечены свободными.
            for i in 0..64u32 {
                std::fs::write(mnt.join(format!("fresh-{i:03}.dat")), b"n".repeat(1024)).unwrap();
            }
            let written = corpus::Corpus::fill(&mnt, 256);
            assert!(written > 16, "записалось всего {written} МиБ");
        }

        let out = std::process::Command::new("xfs_repair").arg("-n").arg(&img).output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        eprintln!("─── xfs_repair -n ПОСЛЕ монтирования ───\n{text}───────────");

        // Настоящие жалобы против строк хода работы: «- moving disconnected
        // inodes to lost+found ...» это заголовок фазы, а не находка, и
        // отличать их обязательно — иначе тест ловит собственный шум.
        let judge = |t: &str| -> Vec<String> {
            t.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .filter(|l| {
                    !l.starts_with("Phase")
                        && !l.starts_with('-')
                        && !l.starts_with("No modify flag set")
                        && !l.contains("host filesystem geometry")
                        && !l.contains("Repair may fail")
                        && !l.contains("sector size mismatch")
                        && !l.contains("the image and the host filesystem")
                })
                .map(|l| l.to_string())
                .collect()
        };
        for (when, t) in [("до монтирования", &pre_text), ("после", &text)] {
            let found = judge(t);
            // Единственный разрешённый остаток — блоки удалённых файлов: они
            // ещё числятся занятыми, и вернёт их D4.
            let unexpected: Vec<&String> =
                found.iter().filter(|l| !l.starts_with("unknown block state")).collect();
            assert!(
                unexpected.is_empty(),
                "xfs_repair {when} нашёл не только остаток D4:\n{}",
                unexpected.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n")
            );
            assert!(
                found.iter().any(|l| l.starts_with("unknown block state")),
                "{when}: ожидали остаток D4 (блоки без владельца), а его нет — \
                 значит тест перестал что-либо различать"
            );
        }
        let complaints = corpus::kernel_complaints(before, &["XFS", "xfs"]);
        let bad: Vec<&String> =
            complaints.iter().filter(|l| l.contains("error") || l.contains("corrupt")).collect();
        assert!(bad.is_empty(), "ядро ругалось на ФС:\n{bad:?}");
    }
}
