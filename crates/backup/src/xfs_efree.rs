//! XFS, шаг D4: вернуть блоки удалённого файла в свободное место.
//!
//! Последний шаг связки D2 (имя) → D3 (инод) → D4 (блоки). После него ФС
//! согласована: `xfs_repair` не находит ничего. Связку собирает
//! [`crate::xfs_stage2::plan_stage2`] (D5) — он же решает, кого щадить.
//!
//! Самый опасный шаг этапа — здесь живут настоящие способы испортить ФС, — и
//! поэтому он устроен как можно скучнее.
//!
//! ## Что делает ядро (снято пробой, не выведено из головы)
//!
//! `scripts/xfs-free-delta.py` удаляет дюжину файлов вперемешку с
//! сохраняемыми и показывает не только какие записи изменились, но и **не
//! понадобились ли новые блоки деревьев**. Ответ на обычном удалении: нет.
//! Корни и уровни BNOBT/CNTBT/RMAPBT остались прежними, `agf_flcount` не
//! шевельнулся, `agf_btreeblks` тоже. То есть AGFL трогать не нужно, и вся
//! работа — правка внутри существующих листьев.
//!
//! Меняются: записи BNOBT и CNTBT (у нас их стало на одиннадцать больше при
//! двенадцати возвращённых экстентах — один слился с соседним свободным),
//! записи RMAPBT исчезают вместе с инодом, `agf_freeblks`, `agf_longest` и
//! `sb_fdblocks`.
//!
//! ## Как устроен writer
//!
//! Не «поправить запись на месте», а **пересчитать набор свободных экстентов
//! целиком и применить разницу**. Читаем все записи BNOBT, добавляем
//! возвращаемые куски, сливаем соседние, сравниваем со старым набором и
//! получаем два списка: что удалить и что вставить. Дальше одна и та же пара
//! операций применяется к обоим деревьям — BNOBT по номеру блока, CNTBT по
//! длине. Так не нужно разбирать четыре случая слияния (слева, справа, с
//! обеих сторон, ни с одной) в каждом дереве по отдельности, а это ровно то
//! место, где легко ошибиться незаметно.
//!
//! ## Границы
//!
//! - Вставка и удаление — только В ПРЕДЕЛАХ ЛИСТА. Разбиение узла, слияние
//!   листьев и уменьшение высоты дерева не делаются: такой экстент щадится, и
//!   файл остаётся на Stage 1.
//! - RMAPBT — перекрывающееся дерево: во внутреннем узле ДВА ключа на слот,
//!   низкий и высокий. Высокий пересчитывается ДО КОРНЯ после каждого
//!   удаления, а не «пока правим нулевой слот»: он мог измениться на любом
//!   уровне, даже когда первый ключ поддерева остался прежним. Одноуровневым
//!   ограничиться не вышло — на 768 МиБ и восьмистах файлах дерево уже
//!   двухуровневое.

use std::collections::{BTreeMap, BTreeSet};

use own_backup_common::{tr, OwnBackupError, Result};

use crate::fat_map::RawVolume;
use crate::xfs_dirent::{crc_matches, set_crc};
use crate::xfs_exclusions::Sb;
use crate::xfs_stage2::{AgReader, MAGIC_BNO, MAGIC_CNT, MAGIC_RMAP};

fn bad(msg: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::Device(format!("xfs efree: {msg}"))
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

const BTREE_HDR: usize = 56;
const BTREE_CRC_OFF: usize = 0x34;

/// Поля AGF.
const AGF_FLCOUNT: usize = 0x30;
const AGF_FREEBLKS: usize = 0x34;
const AGF_LONGEST: usize = 0x38;
/// CRC у AGF лежит ПОСЛЕ длинного хвоста запасных полей, по 0xD8. По 0x50
/// начинается `agf_rmap_blocks`, и первая версия ошиблась именно так —
/// поймал сторож, который сверяет нашу формулу с уже записанной суммой ДО
/// того, как что-то будет записано. Без него правка ушла бы в AGF с
/// разрушенной контрольной суммой.
const AGF_CRC_OFF: usize = 0xD8;
/// Поле суперблока.

/// Дерево группы, которое умеет править этот модуль.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    /// Свободное место по номеру блока.
    Bno,
    /// Свободное место по длине.
    Cnt,
    /// Обратные ссылки.
    Rmap,
}

impl Kind {
    fn magic(self) -> &'static [u8; 4] {
        match self {
            Kind::Bno => MAGIC_BNO,
            Kind::Cnt => MAGIC_CNT,
            Kind::Rmap => MAGIC_RMAP,
        }
    }
    fn reclen(self) -> usize {
        match self {
            Kind::Bno | Kind::Cnt => 8,
            Kind::Rmap => 24,
        }
    }
    /// Ширина слота ключа в узле. У перекрывающегося дерева ключей два.
    fn keyslot(self) -> usize {
        match self {
            Kind::Bno | Kind::Cnt => 8,
            Kind::Rmap => 40,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Kind::Bno => "BNOBT",
            Kind::Cnt => "CNTBT",
            Kind::Rmap => "RMAPBT",
        }
    }
}

/// Свободный экстент.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Free {
    pub(crate) agbno: u64,
    pub(crate) len: u64,
}

/// Почему экстент остался занятым.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExtRefusal {
    /// Лист полон — вставка потребовала бы разбиения.
    LeafFull { ag: u64, tree: &'static str },
    /// Удаление увело бы лист ниже минимума — потребовалось бы слияние.
    LeafUnderflow { ag: u64, tree: &'static str },
}

impl std::fmt::Display for ExtRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExtRefusal::LeafFull { ag, tree } => {
                f.write_str(&tr!("xfs_efree.refusal_leaf_full", ag = ag, tree = tree))
            }
            ExtRefusal::LeafUnderflow { ag, tree } => f.write_str(&tr!(
                "xfs_efree.refusal_leaf_underflow",
                ag = ag,
                tree = tree
            )),
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct ExtentFreeing {
    pub(crate) patches: Vec<(u64, Vec<u8>)>,
    /// Сколько блоков вернулось в свободное место — оно же приращение
    /// `sb_fdblocks`. Суперблок здесь не патчится: он общий с D3, см.
    /// [`crate::xfs_ifree::patch_superblock`].
    pub(crate) blocks_freed: u64,
    pub(crate) refusals: Vec<ExtRefusal>,
}

/// Блоки одной группы, правленные в памяти.
struct AgEdits {
    ag: u64,
    blocks: BTreeMap<u64, Vec<u8>>,
}

impl AgEdits {
    fn get(
        &mut self,
        vol: &mut dyn RawVolume,
        sb: &Sb,
        agbno: u64,
        kind: Kind,
    ) -> Result<&mut Vec<u8>> {
        if !self.blocks.contains_key(&agbno) {
            let blk = AgReader::new(sb, self.ag).read_block_at(vol, agbno)?;
            if &blk[0..4] != kind.magic() {
                return Err(bad(tr!(
                    "xfs_efree.block_wrong_tree",
                    ag = self.ag,
                    agbno = agbno,
                    tree = kind.name(),
                    magic = format!("{:?}", &blk[0..4])
                )));
            }
            if !crc_matches(&blk, BTREE_CRC_OFF) {
                return Err(bad(tr!(
                    "xfs_efree.block_crc_mismatch",
                    ag = self.ag,
                    agbno = agbno,
                    tree = kind.name()
                )));
            }
            self.blocks.insert(agbno, blk);
        }
        Ok(self.blocks.get_mut(&agbno).unwrap())
    }
}

fn leaf_maxrecs(sb: &Sb, kind: Kind) -> usize {
    (sb.blocksize as usize - BTREE_HDR) / kind.reclen()
}
fn node_maxrecs(sb: &Sb, kind: Kind) -> usize {
    (sb.blocksize as usize - BTREE_HDR) / (kind.keyslot() + 4)
}
/// Минимум записей в НЕкорневом блоке. Корень может быть сколь угодно пуст.
///
/// Проверено опытом, а не выведено: если позволить листу RMAPBT опуститься до
/// одной записи, `xfs_repair` немедленно говорит
/// «bad btree nrecs (1, min=84, max=168) in btrmap block 0/7». Ядро на чтении
/// этого не замечает, но образ считается битым — значит минимум обязателен, а
/// путь к массовому удалению лежит через слияние листьев.
fn minrecs(sb: &Sb, kind: Kind, leaf: bool) -> usize {
    if leaf {
        leaf_maxrecs(sb, kind) / 2
    } else {
        node_maxrecs(sb, kind) / 2
    }
}

/// Порядок сравнения ключа из БЛОКА с искомым.
///
/// У BNOBT ключ — номер блока; у CNTBT — сначала длина, потом номер (структура
/// та же, а сравнение другое, и перепутать их значит уехать в чужое поддерево).
fn cmp_alloc(kind: Kind, key_bytes: &[u8], target: Free) -> std::cmp::Ordering {
    let start = be32(key_bytes, 0);
    let len = be32(key_bytes, 4);
    match kind {
        Kind::Bno => start.cmp(&target.agbno),
        Kind::Cnt => len.cmp(&target.len).then(start.cmp(&target.agbno)),
        Kind::Rmap => unreachable!("cmp_alloc не для rmap"),
    }
}

// --- RMAPBT: перекрывающееся дерево ------------------------------------------
//
// У него во внутреннем узле ДВА ключа на слот: низкий (первый ключ поддерева) и
// высокий (максимальный «конец» в поддереве). Высокий и делает дерево
// перекрывающимся — по нему ядро отсекает поддеревья при поиске по диапазону.
// Считать указатели по одному ключу значит уехать в чужие байты; на этом уже
// спотыкались в D1, и первый же обход упирался в суперблок.

/// Ключ rmap: `rm_startblock`(4) + `rm_owner`(8) + `rm_offset`(8).
type RKey = [u8; 20];
/// Флаги живут в старших битах `rm_offset` и в сравнении не участвуют.
const RMAP_OFF_MASK: u64 = 0x003F_FFFF_FFFF_FFFF;
/// Владелец «не инод» закодирован старшим битом (в ядре это отрицательное i64).
const RMAP_NON_INODE: u64 = 1 << 63;
/// Смещение не наращивается для форка атрибутов и блоков bmbt.
const RMAP_OFF_UNADJUSTABLE: u64 = (1 << 63) | (1 << 62);

fn rmap_low_key(rec: &[u8]) -> RKey {
    let mut k = [0u8; 20];
    k[0..4].copy_from_slice(&rec[0..4]);
    k[4..12].copy_from_slice(&rec[8..16]);
    k[12..20].copy_from_slice(&rec[16..24]);
    k
}

/// Высокий ключ записи — её последний блок и последнее смещение.
fn rmap_high_key(rec: &[u8]) -> RKey {
    let start = be32(rec, 0);
    let count = be32(rec, 4);
    let owner = be64(rec, 8);
    let offset = be64(rec, 16);
    let adj = count.saturating_sub(1);
    let mut k = [0u8; 20];
    put_be32(&mut k, 0, (start + adj) as u32);
    k[4..12].copy_from_slice(&owner.to_be_bytes());
    let keep = owner & RMAP_NON_INODE != 0 || offset & RMAP_OFF_UNADJUSTABLE != 0;
    let off = if keep { offset } else { offset + adj };
    k[12..20].copy_from_slice(&off.to_be_bytes());
    k
}

fn rmap_key_cmp(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    be32(a, 0)
        .cmp(&be32(b, 0))
        .then_with(|| be64(a, 4).cmp(&be64(b, 4)))
        .then_with(|| (be64(a, 12) & RMAP_OFF_MASK).cmp(&(be64(b, 12) & RMAP_OFF_MASK)))
}

/// Пара ключей блока: низкий — первый, высокий — максимум по всему блоку.
fn rmap_block_keys(blk: &[u8]) -> Result<(RKey, RKey)> {
    let level = be16(blk, 0x04);
    let n = be16(blk, 0x06) as usize;
    if n == 0 {
        return Err(bad(tr!("xfs_efree.rmap_block_empty")));
    }
    let mut low = [0u8; 20];
    let mut high = [0u8; 20];
    if level == 0 {
        let first = &blk[BTREE_HDR..BTREE_HDR + 24];
        low = rmap_low_key(first);
        high = rmap_high_key(first);
        for i in 1..n {
            let o = BTREE_HDR + i * 24;
            let h = rmap_high_key(&blk[o..o + 24]);
            if rmap_key_cmp(&h, &high) == std::cmp::Ordering::Greater {
                high = h;
            }
        }
    } else {
        low.copy_from_slice(&blk[BTREE_HDR..BTREE_HDR + 20]);
        high.copy_from_slice(&blk[BTREE_HDR + 20..BTREE_HDR + 40]);
        for i in 1..n {
            let o = BTREE_HDR + i * 40 + 20;
            if rmap_key_cmp(&blk[o..o + 20], &high) == std::cmp::Ordering::Greater {
                high.copy_from_slice(&blk[o..o + 20]);
            }
        }
    }
    Ok((low, high))
}

/// Спуск в RMAPBT по низким ключам.
fn descend_rmap(
    edits: &mut AgEdits,
    vol: &mut dyn RawVolume,
    sb: &Sb,
    root: u64,
    target: &RKey,
) -> Result<Located> {
    let mut path = Vec::new();
    let mut agbno = root;
    for _ in 0..16 {
        let nmax = node_maxrecs(sb, Kind::Rmap);
        let blk = edits.get(vol, sb, agbno, Kind::Rmap)?;
        let level = be16(blk, 0x04);
        let n = be16(blk, 0x06) as usize;
        if level == 0 {
            let mut idx = n;
            let mut found = false;
            for i in 0..n {
                let o = BTREE_HDR + i * 24;
                let k = rmap_low_key(&blk[o..o + 24]);
                match rmap_key_cmp(&k, target) {
                    std::cmp::Ordering::Equal => {
                        idx = i;
                        found = true;
                        break;
                    }
                    std::cmp::Ordering::Greater => {
                        idx = i;
                        break;
                    }
                    std::cmp::Ordering::Less => {}
                }
            }
            return Ok(Located {
                path,
                leaf: agbno,
                idx,
                found,
            });
        }
        if n == 0 || n > nmax {
            return Err(bad(tr!("xfs_efree.rmap_node_impossible")));
        }
        let ptr_off = BTREE_HDR + nmax * 40;
        let mut slot = 0usize;
        for i in 0..n {
            let o = BTREE_HDR + i * 40;
            if rmap_key_cmp(&blk[o..o + 20], target) != std::cmp::Ordering::Greater {
                slot = i;
            } else {
                break;
            }
        }
        let child = be32(blk, ptr_off + slot * 4);
        path.push((agbno, slot));
        agbno = child;
    }
    Err(bad(tr!("xfs_efree.rmap_too_deep")))
}

/// Пересчитать пару ключей у всех предков.
///
/// У перекрывающегося дерева подниматься надо ДО КОРНЯ, а не «пока правим
/// нулевой слот»: высокий ключ мог измениться на любом уровне, даже если
/// первый ключ поддерева остался прежним.
fn rmap_update_keys_upward(
    edits: &mut AgEdits,
    vol: &mut dyn RawVolume,
    sb: &Sb,
    path: &[(u64, usize)],
    leaf: u64,
) -> Result<()> {
    let mut child = leaf;
    for &(agbno, slot) in path.iter().rev() {
        let (low, high) = {
            let blk = edits.get(vol, sb, child, Kind::Rmap)?;
            rmap_block_keys(blk)?
        };
        let blk = edits.get(vol, sb, agbno, Kind::Rmap)?;
        let o = BTREE_HDR + slot * 40;
        blk[o..o + 20].copy_from_slice(&low);
        blk[o + 20..o + 40].copy_from_slice(&high);
        child = agbno;
    }
    Ok(())
}

/// Собрать все записи RMAPBT вместе с блоком, где они лежат.
fn read_rmap_recs(
    edits: &mut AgEdits,
    vol: &mut dyn RawVolume,
    sb: &Sb,
    root: u64,
) -> Result<Vec<[u8; 24]>> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    let mut seen = 0;
    while let Some(agbno) = stack.pop() {
        seen += 1;
        if seen > 8192 {
            return Err(bad(tr!("xfs_efree.rmap_too_large")));
        }
        let nmax = node_maxrecs(sb, Kind::Rmap);
        let blk = edits.get(vol, sb, agbno, Kind::Rmap)?;
        let level = be16(blk, 0x04);
        let n = be16(blk, 0x06) as usize;
        if level == 0 {
            for i in 0..n {
                let o = BTREE_HDR + i * 24;
                let mut r = [0u8; 24];
                r.copy_from_slice(&blk[o..o + 24]);
                out.push(r);
            }
        } else {
            let ptr_off = BTREE_HDR + nmax * 40;
            for i in 0..n {
                stack.push(be32(blk, ptr_off + i * 4));
            }
        }
    }
    Ok(out)
}

/// Спуск к листу по ключу. Возвращает дорогу, лист и место записи.
struct Located {
    path: Vec<(u64, usize)>,
    leaf: u64,
    idx: usize,
    found: bool,
}

fn descend_alloc(
    edits: &mut AgEdits,
    vol: &mut dyn RawVolume,
    sb: &Sb,
    root: u64,
    kind: Kind,
    target: Free,
) -> Result<Located> {
    let mut path = Vec::new();
    let mut agbno = root;
    for _ in 0..16 {
        let nmax = node_maxrecs(sb, kind);
        let blk = edits.get(vol, sb, agbno, kind)?;
        let level = be16(blk, 0x04);
        let nrecs = be16(blk, 0x06) as usize;
        if level == 0 {
            if nrecs > leaf_maxrecs(sb, kind) {
                return Err(bad(tr!("xfs_efree.leaf_impossible", tree = kind.name())));
            }
            let mut idx = nrecs;
            let mut found = false;
            for i in 0..nrecs {
                let o = BTREE_HDR + i * kind.reclen();
                match cmp_alloc(kind, &blk[o..o + 8], target) {
                    std::cmp::Ordering::Equal => {
                        idx = i;
                        found = true;
                        break;
                    }
                    std::cmp::Ordering::Greater => {
                        idx = i;
                        break;
                    }
                    std::cmp::Ordering::Less => {}
                }
            }
            return Ok(Located {
                path,
                leaf: agbno,
                idx,
                found,
            });
        }
        if nrecs == 0 || nrecs > nmax {
            return Err(bad(tr!("xfs_efree.node_impossible", tree = kind.name())));
        }
        let ptr_off = BTREE_HDR + nmax * kind.keyslot();
        let mut slot = 0usize;
        for i in 0..nrecs {
            let o = BTREE_HDR + i * kind.keyslot();
            if cmp_alloc(kind, &blk[o..o + 8], target) != std::cmp::Ordering::Greater {
                slot = i;
            } else {
                break;
            }
        }
        let child = be32(blk, ptr_off + slot * 4);
        path.push((agbno, slot));
        agbno = child;
    }
    Err(bad(tr!("xfs_efree.tree_too_deep")))
}

/// Вставить запись в лист; `false` — не влезла.
fn leaf_insert(blk: &mut [u8], sb: &Sb, kind: Kind, idx: usize, rec: &[u8]) -> bool {
    let n = be16(blk, 0x06) as usize;
    if n + 1 > leaf_maxrecs(sb, kind) {
        return false;
    }
    let rl = kind.reclen();
    let base = BTREE_HDR + idx * rl;
    let end = BTREE_HDR + n * rl;
    blk.copy_within(base..end, base + rl);
    blk[base..base + rl].copy_from_slice(rec);
    put_be16(blk, 0x06, (n + 1) as u16);
    true
}

/// Убрать запись из листа; `false` — ушли бы ниже минимума.
fn leaf_delete(blk: &mut [u8], sb: &Sb, kind: Kind, idx: usize, is_root: bool) -> bool {
    let n = be16(blk, 0x06) as usize;
    if idx >= n {
        return false;
    }
    if !is_root && n - 1 < minrecs(sb, kind, true) {
        return false;
    }
    let rl = kind.reclen();
    let base = BTREE_HDR + idx * rl;
    let end = BTREE_HDR + n * rl;
    blk.copy_within(base + rl..end, base);
    blk[end - rl..end].fill(0);
    put_be16(blk, 0x06, (n - 1) as u16);
    true
}

/// Скопировать первую запись листа в ключ родителя, поднимаясь, пока правим
/// нулевой слот.
fn refresh_parent_keys(
    edits: &mut AgEdits,
    vol: &mut dyn RawVolume,
    sb: &Sb,
    kind: Kind,
    path: &[(u64, usize)],
    leaf: u64,
) -> Result<()> {
    let mut child = leaf;
    for &(agbno, slot) in path.iter().rev() {
        let first = {
            let blk = edits.get(vol, sb, child, kind)?;
            let n = be16(blk, 0x06) as usize;
            if n == 0 {
                return Err(bad(tr!(
                    "xfs_efree.block_empty",
                    tree = kind.name(),
                    child = child
                )));
            }
            let level = be16(blk, 0x04);
            // У листа ключ берётся из записи, у узла — из его первого ключа.
            let o = if level == 0 { BTREE_HDR } else { BTREE_HDR };
            let mut k = [0u8; 8];
            k.copy_from_slice(&blk[o..o + 8]);
            k
        };
        let blk = edits.get(vol, sb, agbno, kind)?;
        blk[BTREE_HDR + slot * kind.keyslot()..BTREE_HDR + slot * kind.keyslot() + 8]
            .copy_from_slice(&first);
        if slot != 0 {
            break;
        }
        child = agbno;
    }
    Ok(())
}

/// Прочитать все записи дерева свободного места (обход листьев).
fn read_alloc_recs(
    edits: &mut AgEdits,
    vol: &mut dyn RawVolume,
    sb: &Sb,
    root: u64,
    kind: Kind,
) -> Result<Vec<Free>> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    let mut seen = 0;
    while let Some(agbno) = stack.pop() {
        seen += 1;
        if seen > 4096 {
            return Err(bad(tr!("xfs_efree.tree_too_large")));
        }
        let nmax = node_maxrecs(sb, kind);
        let blk = edits.get(vol, sb, agbno, kind)?;
        let level = be16(blk, 0x04);
        let n = be16(blk, 0x06) as usize;
        if level == 0 {
            for i in 0..n {
                let o = BTREE_HDR + i * kind.reclen();
                out.push(Free {
                    agbno: be32(blk, o),
                    len: be32(blk, o + 4),
                });
            }
        } else {
            let ptr_off = BTREE_HDR + nmax * kind.keyslot();
            for i in 0..n {
                stack.push(be32(blk, ptr_off + i * 4));
            }
        }
    }
    out.sort_unstable();
    Ok(out)
}

/// Слить набор свободных экстентов с возвращаемыми.
///
/// Отдельная чистая функция — её и проверяют тесты: слияние слева, справа, с
/// обеих сторон и ни с одной.
pub(crate) fn merge_free(old: &[Free], returned: &[Free]) -> Vec<Free> {
    let mut all: Vec<Free> = old.iter().chain(returned.iter()).copied().collect();
    all.sort_unstable();
    let mut out: Vec<Free> = Vec::with_capacity(all.len());
    for f in all {
        match out.last_mut() {
            Some(last) if last.agbno + last.len == f.agbno => last.len += f.len,
            Some(last) if f.agbno < last.agbno + last.len => {
                // Перекрытие означает, что мы возвращаем уже свободный блок —
                // это порча, а не краевой случай.
                out.push(f);
            }
            _ => out.push(f),
        }
    }
    out
}

/// Вернуть блоки в свободное место.
///
/// `returned` — куски по группам: (AG, agbno, длина). `owners` — иноды, чьи
/// записи RMAPBT надо убрать.
pub(crate) fn plan_extent_freeing(
    vol: &mut dyn RawVolume,
    sb: &Sb,
    returned: &BTreeMap<u64, Vec<Free>>,
    owners: &BTreeSet<u64>,
) -> Result<ExtentFreeing> {
    let mut out = ExtentFreeing::default();
    let mut fdblocks_delta: u64 = 0;

    for (&ag, runs) in returned {
        if runs.is_empty() {
            continue;
        }
        let reader = AgReader::new(sb, ag);
        let agf = reader.agf(vol)?;
        let mut bno_edits = AgEdits {
            ag,
            blocks: BTreeMap::new(),
        };
        let mut cnt_edits = AgEdits {
            ag,
            blocks: BTreeMap::new(),
        };

        let old = read_alloc_recs(&mut bno_edits, vol, sb, agf.bno_root, Kind::Bno)?;
        // Возвращать уже свободный блок нельзя — это признак, что где-то выше
        // посчитали не то, и молча продолжать здесь опаснее, чем отказать.
        for r in runs {
            if old
                .iter()
                .any(|f| r.agbno < f.agbno + f.len && f.agbno < r.agbno + r.len)
            {
                return Err(bad(tr!(
                    "xfs_efree.extent_already_free",
                    ag = ag,
                    start = r.agbno,
                    len = r.len
                )));
            }
        }
        let new = merge_free(&old, runs);

        let old_set: BTreeSet<Free> = old.iter().copied().collect();
        let new_set: BTreeSet<Free> = new.iter().copied().collect();
        let to_delete: Vec<Free> = old_set.difference(&new_set).copied().collect();
        let to_insert: Vec<Free> = new_set.difference(&old_set).copied().collect();

        let mut failed = false;
        for kind in [Kind::Bno, Kind::Cnt] {
            let (root, edits) = match kind {
                Kind::Bno => (agf.bno_root, &mut bno_edits),
                _ => (agf.cnt_root, &mut cnt_edits),
            };
            for f in &to_delete {
                let loc = descend_alloc(edits, vol, sb, root, kind, *f)?;
                if !loc.found {
                    return Err(bad(tr!(
                        "xfs_efree.record_missing",
                        ag = ag,
                        tree = kind.name(),
                        start = f.agbno,
                        len = f.len
                    )));
                }
                let is_root = loc.leaf == root;
                let blk = edits.get(vol, sb, loc.leaf, kind)?;
                if !leaf_delete(blk, sb, kind, loc.idx, is_root) {
                    out.refusals.push(ExtRefusal::LeafUnderflow {
                        ag,
                        tree: kind.name(),
                    });
                    failed = true;
                    break;
                }
                if loc.idx == 0 && !loc.path.is_empty() {
                    refresh_parent_keys(edits, vol, sb, kind, &loc.path, loc.leaf)?;
                }
            }
            if failed {
                break;
            }
            for f in &to_insert {
                let loc = descend_alloc(edits, vol, sb, root, kind, *f)?;
                if loc.found {
                    return Err(bad(tr!(
                        "xfs_efree.record_exists",
                        ag = ag,
                        tree = kind.name(),
                        start = f.agbno,
                        len = f.len
                    )));
                }
                let mut rec = [0u8; 8];
                put_be32(&mut rec, 0, f.agbno as u32);
                put_be32(&mut rec, 4, f.len as u32);
                let is_root = loc.leaf == root;
                let _ = is_root;
                let blk = edits.get(vol, sb, loc.leaf, kind)?;
                if !leaf_insert(blk, sb, kind, loc.idx, &rec) {
                    out.refusals.push(ExtRefusal::LeafFull {
                        ag,
                        tree: kind.name(),
                    });
                    failed = true;
                    break;
                }
                if loc.idx == 0 && !loc.path.is_empty() {
                    refresh_parent_keys(edits, vol, sb, kind, &loc.path, loc.leaf)?;
                }
            }
            if failed {
                break;
            }
        }
        if failed {
            // Ничего из этой группы не пишем: полуправленное дерево хуже, чем
            // нетронутое.
            continue;
        }

        // RMAPBT: убрать записи освобождённых инодов.
        let mut rmap_edits = AgEdits {
            ag,
            blocks: BTreeMap::new(),
        };
        let mut rmap_touched = false;
        if agf.rmap_root != 0 && !owners.is_empty() {
            let recs = read_rmap_recs(&mut rmap_edits, vol, sb, agf.rmap_root)?;
            let doomed: Vec<[u8; 24]> = recs
                .into_iter()
                .filter(|r| owners.contains(&be64(r, 8)))
                .collect();
            let mut failed_rmap = false;
            for rec in &doomed {
                let key = rmap_low_key(rec);
                let loc = descend_rmap(&mut rmap_edits, vol, sb, agf.rmap_root, &key)?;
                if !loc.found {
                    return Err(bad(tr!(
                        "xfs_efree.rmap_owner_missing",
                        ag = ag,
                        owner = be64(rec, 8)
                    )));
                }
                let is_root = loc.leaf == agf.rmap_root;
                let blk = rmap_edits.get(vol, sb, loc.leaf, Kind::Rmap)?;
                if !leaf_delete(blk, sb, Kind::Rmap, loc.idx, is_root) {
                    out.refusals
                        .push(ExtRefusal::LeafUnderflow { ag, tree: "RMAPBT" });
                    failed_rmap = true;
                    break;
                }
                rmap_touched = true;
                if !loc.path.is_empty() {
                    rmap_update_keys_upward(&mut rmap_edits, vol, sb, &loc.path, loc.leaf)?;
                }
            }
            if failed_rmap {
                continue;
            }
        }

        // Счётчики группы: пересчитываем по НОВОМУ набору, а не прибавляем
        // дельту — так расхождение не может накопиться.
        let freeblks: u64 = new.iter().map(|f| f.len).sum();
        let longest = new.iter().map(|f| f.len).max().unwrap_or(0);
        let mut agf_sec = reader.read_sector_at(vol, 1)?;
        if !crc_matches(&agf_sec, AGF_CRC_OFF) {
            return Err(bad(tr!("xfs_efree.agf_crc_mismatch", ag = ag)));
        }
        let old_freeblks = be32(&agf_sec, AGF_FREEBLKS);
        // Блоки из AGFL в BNOBT не лежат, но в agf_freeblks и не считаются —
        // сверяем, что наш пересчёт совпал со старым значением плюс возврат.
        let returned_here: u64 = runs.iter().map(|f| f.len).sum();
        if freeblks != old_freeblks + returned_here {
            return Err(bad(tr!(
                "xfs_efree.free_recount_mismatch",
                ag = ag,
                freeblks = freeblks,
                expected = old_freeblks + returned_here
            )));
        }
        put_be32(&mut agf_sec, AGF_FREEBLKS, freeblks as u32);
        put_be32(&mut agf_sec, AGF_LONGEST, longest as u32);
        set_crc(&mut agf_sec, AGF_CRC_OFF);
        out.patches.push((reader.sector_offset(1), agf_sec));

        for (agbno, mut blk) in bno_edits.blocks {
            set_crc(&mut blk, BTREE_CRC_OFF);
            out.patches.push((reader.block_offset(agbno), blk));
        }
        for (agbno, mut blk) in cnt_edits.blocks {
            set_crc(&mut blk, BTREE_CRC_OFF);
            out.patches.push((reader.block_offset(agbno), blk));
        }
        if rmap_touched {
            for (agbno, mut blk) in rmap_edits.blocks {
                set_crc(&mut blk, BTREE_CRC_OFF);
                out.patches.push((reader.block_offset(agbno), blk));
            }
        }
        out.blocks_freed += returned_here;
        fdblocks_delta += returned_here;
    }

    debug_assert_eq!(fdblocks_delta, out.blocks_freed);
    let _ = AGF_FLCOUNT;
    Ok(out)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn f(agbno: u64, len: u64) -> Free {
        Free { agbno, len }
    }

    /// Слияние — четыре случая, и каждый обязан быть проверен: план D1 считал
    /// вставку редкостью, а эталон ядра показал обратное.
    #[test]
    fn merging_covers_all_four_neighbourhoods() {
        // Ни слева, ни справа.
        assert_eq!(
            merge_free(&[f(10, 5), f(100, 5)], &[f(50, 5)]),
            vec![f(10, 5), f(50, 5), f(100, 5)]
        );
        // Слева.
        assert_eq!(merge_free(&[f(10, 5)], &[f(15, 5)]), vec![f(10, 10)]);
        // Справа.
        assert_eq!(merge_free(&[f(20, 5)], &[f(15, 5)]), vec![f(15, 10)]);
        // С обеих сторон — три записи схлопываются в одну.
        assert_eq!(
            merge_free(&[f(10, 5), f(20, 5)], &[f(15, 5)]),
            vec![f(10, 15)]
        );
        // Несколько возвращаемых подряд сливаются и между собой.
        assert_eq!(
            merge_free(&[], &[f(10, 5), f(15, 5), f(20, 5)]),
            vec![f(10, 15)]
        );
    }

    fn test_sb() -> Sb {
        Sb {
            blocksize: 4096,
            logstart: 0,
            logblocks: 0,
            inodesize: 512,
            agblocks: 65536,
            agcount: 4,
            agblklog: 16,
            inopblog: 3,
            rootino: 128,
            sectsize: 512,
            reflink: true,
            ftype: true,
            spinodes: true,
        }
    }

    fn leaf(kind: Kind, recs: &[(u64, u64)]) -> Vec<u8> {
        let mut b = vec![0u8; 4096];
        b[0..4].copy_from_slice(kind.magic());
        put_be16(&mut b, 0x04, 0);
        put_be16(&mut b, 0x06, recs.len() as u16);
        for (i, (a, l)) in recs.iter().enumerate() {
            let o = BTREE_HDR + i * kind.reclen();
            put_be32(&mut b, o, *a as u32);
            put_be32(&mut b, o + 4, *l as u32);
        }
        b
    }

    /// Вставка и удаление держат порядок и число записей.
    #[test]
    fn leaf_edits_keep_the_order() {
        let sb = test_sb();
        let mut b = leaf(Kind::Bno, &[(10, 1), (30, 1), (50, 1)]);
        let mut rec = [0u8; 8];
        put_be32(&mut rec, 0, 20);
        put_be32(&mut rec, 4, 1);
        assert!(leaf_insert(&mut b, &sb, Kind::Bno, 1, &rec));
        assert_eq!(be16(&b, 0x06), 4);
        let got: Vec<u64> = (0..4).map(|i| be32(&b, BTREE_HDR + i * 8)).collect();
        assert_eq!(got, vec![10, 20, 30, 50]);

        assert!(leaf_delete(&mut b, &sb, Kind::Bno, 0, true));
        assert_eq!(be16(&b, 0x06), 3);
        let got: Vec<u64> = (0..3).map(|i| be32(&b, BTREE_HDR + i * 8)).collect();
        assert_eq!(got, vec![20, 30, 50]);
    }

    /// Полный лист не разбивается, а отказывает; и numrecs при отказе не растёт.
    #[test]
    fn a_full_leaf_refuses() {
        let sb = test_sb();
        let max = leaf_maxrecs(&sb, Kind::Bno);
        let recs: Vec<(u64, u64)> = (0..max as u64).map(|i| (i * 4, 1)).collect();
        let mut b = leaf(Kind::Bno, &recs);
        let rec = [0u8; 8];
        assert!(!leaf_insert(&mut b, &sb, Kind::Bno, 0, &rec));
        assert_eq!(be16(&b, 0x06), max as u64);
    }

    /// Некорневой лист не опускается ниже минимума — иначе понадобилось бы
    /// слияние, которого здесь нет.
    #[test]
    fn a_thin_non_root_leaf_refuses_deletion() {
        let sb = test_sb();
        let min = minrecs(&sb, Kind::Bno, true);
        let recs: Vec<(u64, u64)> = (0..min as u64).map(|i| (i * 4, 1)).collect();
        let mut b = leaf(Kind::Bno, &recs);
        assert!(
            !leaf_delete(&mut b, &sb, Kind::Bno, 0, false),
            "лист ушёл ниже минимума"
        );
        // Тот же лист как КОРЕНЬ — можно: у корня минимума нет.
        assert!(leaf_delete(&mut b, &sb, Kind::Bno, 0, true));
    }

    /// CNTBT сравнивается по ДЛИНЕ, а BNOBT по номеру блока. Перепутать их
    /// значит уехать в чужое поддерево, и это самая дешёвая ошибка здесь.
    #[test]
    fn the_two_trees_order_differently() {
        let mut key = [0u8; 8];
        put_be32(&mut key, 0, 500); // startblock
        put_be32(&mut key, 4, 10); // blockcount
        use std::cmp::Ordering::*;
        assert_eq!(cmp_alloc(Kind::Bno, &key, f(400, 99)), Greater);
        assert_eq!(cmp_alloc(Kind::Bno, &key, f(600, 1)), Less);
        // По длине: 10 против 99 — меньше, хотя номер блока больше.
        assert_eq!(cmp_alloc(Kind::Cnt, &key, f(400, 99)), Less);
        // При равной длине решает номер блока.
        assert_eq!(cmp_alloc(Kind::Cnt, &key, f(400, 10)), Greater);
    }

    /// Приёмка всей связки D2 → D3 → D4 на живой ФС.
    ///
    /// Это первый прогон, где `xfs_repair -n` обязан не найти НИЧЕГО: имя
    /// убрано, инод освобождён, блоки вернулись. Никаких «ожидаемых остатков»
    /// здесь больше нет — если строка появилась, значит что-то не доделано.
    ///
    /// Динамическая половина строже: ядро монтирует, читает всё уцелевшее,
    /// пишет до заполнения — то есть ПЕРЕИСПОЛЬЗУЕТ возвращённые блоки, — и
    /// читает снова.
    #[test]
    #[ignore = "needs root: loop-mounts the patched image and writes to it"]
    fn the_whole_chain_leaves_a_filesystem_repair_calls_clean() {
        use crate::exclusion_corpus as corpus;
        use crate::xfs_dirent::plan_dirent_removal;
        use crate::xfs_exclusions::{fork_extents_at, parse_sb, read_inode};
        use crate::xfs_ifree::{patch_superblock, plan_inode_freeing};
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
                self.f
                    .read_exact_at(buf, offset)
                    .map_err(|e| bad(format!("read: {e}")))
            }
            fn len(&self) -> u64 {
                self.len
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("fs.img");
        std::fs::File::create(&img)
            .unwrap()
            .set_len(768 * 1024 * 1024)
            .unwrap();
        assert!(std::process::Command::new("mkfs.xfs")
            .args(["-q", "-m", "reflink=1,crc=1"])
            .arg(&img)
            .status()
            .unwrap()
            .success());
        let mnt = dir.path().join("mnt");
        std::fs::create_dir_all(&mnt).unwrap();

        // Удаляемые вперемешку с сохраняемыми: возврат экстента почти никогда
        // не примыкает к чужому свободному, и это и есть жизнь.
        let (dirs, kept) = {
            let _m = corpus::mounted(&img, &mnt);
            let mut dirs: Vec<(String, u64, Vec<String>, Vec<u64>)> = Vec::new();
            let mut kept = Vec::new();
            for (name, n, vics) in [("small", 3u32, 1usize), ("dense", 400, 8)] {
                let d = mnt.join(name);
                std::fs::create_dir(&d).unwrap();
                let mut names = Vec::new();
                let mut inos = Vec::new();
                for i in 0..n {
                    std::fs::write(d.join(format!("file-{i:05}.dat")), b"k".repeat(4096)).unwrap();
                    kept.push((name.to_string(), format!("file-{i:05}.dat")));
                    if (i as usize) < vics {
                        let v = format!("VICTIM-{i}-long-name.dat");
                        std::fs::write(d.join(&v), b"v".repeat(256 * 1024 * (i as usize + 1)))
                            .unwrap();
                        inos.push(std::fs::metadata(d.join(&v)).unwrap().st_ino());
                        names.push(v);
                    }
                }
                let dino = std::fs::metadata(&d).unwrap().st_ino();
                dirs.push((name.to_string(), dino, names, inos));
            }
            (dirs, kept)
        };

        let fh = std::fs::File::open(&img).unwrap();
        let len = fh.metadata().unwrap().len();
        let mut vol = FileVol { f: fh, len };
        let sb = parse_sb(&mut vol).unwrap();
        let mask = (1u64 << sb.agblklog) - 1;

        // Экстенты читаются ДО D3 — он обнуляет `di_nextents`.
        let mut returned: BTreeMap<u64, Vec<Free>> = BTreeMap::new();
        let mut owners: BTreeSet<u64> = BTreeSet::new();
        let mut all_inos = Vec::new();
        for (_, _, _, inos) in &dirs {
            for &ino in inos {
                let inode = read_inode(&mut vol, &sb, ino).unwrap();
                let nextents = crate::xfs_exclusions::data_nextents(&inode);
                for (_, e) in fork_extents_at(&mut vol, &sb, &inode, inode[5], nextents).unwrap() {
                    returned
                        .entry(e.startblock >> sb.agblklog)
                        .or_default()
                        .push(Free {
                            agbno: e.startblock & mask,
                            len: e.count,
                        });
                }
                owners.insert(ino);
                all_inos.push(ino);
            }
        }
        for v in returned.values_mut() {
            v.sort_unstable();
        }
        let total_blocks: u64 = returned.values().flatten().map(|f| f.len).sum();

        let mut patches: Vec<(u64, Vec<u8>)> = Vec::new();
        for (name, dino, names, _) in &dirs {
            let edit = plan_dirent_removal(&mut vol, &sb, *dino, names).unwrap();
            assert!(edit.refusals.is_empty(), "D2 в {name}: {:?}", edit.refusals);
            patches.extend(edit.patches);
        }
        let freeing = plan_inode_freeing(&mut vol, &sb, &all_inos).unwrap();
        assert!(
            freeing.refusals.is_empty(),
            "D3 пощадил: {:?}",
            freeing
                .refusals
                .iter()
                .map(|r| r.to_string())
                .collect::<Vec<_>>()
        );
        patches.extend(freeing.patches);

        let ext = plan_extent_freeing(&mut vol, &sb, &returned, &owners).unwrap();
        assert!(
            ext.refusals.is_empty(),
            "D4 пощадил: {:?}",
            ext.refusals
                .iter()
                .map(|r| r.to_string())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            ext.blocks_freed, total_blocks,
            "вернулось блоков не столько"
        );
        patches.extend(ext.patches);

        if let Some(sbp) =
            patch_superblock(&mut vol, &sb, freeing.sb_ifree_delta, ext.blocks_freed).unwrap()
        {
            patches.push(sbp);
        }
        eprintln!(
            "  инодов {}, блоков {}, патчей {}",
            all_inos.len(),
            total_blocks,
            patches.len()
        );
        drop(vol);

        let w = std::fs::OpenOptions::new().write(true).open(&img).unwrap();
        for (off, bytes) in &patches {
            w.write_all_at(bytes, *off).unwrap();
        }
        w.sync_all().unwrap();
        drop(w);

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
        let repair = |img: &std::path::Path| -> String {
            let o = std::process::Command::new("xfs_repair")
                .arg("-n")
                .arg(img)
                .output()
                .unwrap();
            format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            )
        };

        let before_mount = repair(&img);
        let found = judge(&before_mount);
        assert!(
            found.is_empty(),
            "после D2+D3+D4 xfs_repair обязан молчать, а он сказал:\n{}",
            found.join("\n")
        );

        // Ядро.
        let dmesg = corpus::dmesg_len();
        {
            let _m = corpus::mounted(&img, &mnt);
            for (name, _, names, _) in &dirs {
                let listed: std::collections::HashSet<String> = std::fs::read_dir(mnt.join(name))
                    .unwrap()
                    .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
                    .collect();
                for n in names {
                    assert!(!listed.contains(n), "{name}: имя {n} осталось");
                }
            }
            for (d, fname) in &kept {
                let body = std::fs::read(mnt.join(d).join(fname)).unwrap();
                assert_eq!(body.len(), 4096, "сосед {d}/{fname} прочитался иначе");
            }
            let written = corpus::Corpus::fill(&mnt, 512);
            assert!(written > 32, "записалось всего {written} МиБ");
            for (d, fname) in &kept {
                let body = std::fs::read(mnt.join(d).join(fname)).unwrap();
                assert_eq!(body.len(), 4096, "после заполнения {d}/{fname} испортился");
            }
        }

        let after = repair(&img);
        let found = judge(&after);
        assert!(
            found.is_empty(),
            "после монтирования и записи xfs_repair сказал:\n{}",
            found.join("\n")
        );
        let complaints = corpus::kernel_complaints(dmesg, &["XFS", "xfs"]);
        let bad_lines: Vec<&String> = complaints
            .iter()
            .filter(|l| l.contains("error") || l.contains("corrupt"))
            .collect();
        assert!(bad_lines.is_empty(), "ядро ругалось:\n{bad_lines:?}");
    }
}
