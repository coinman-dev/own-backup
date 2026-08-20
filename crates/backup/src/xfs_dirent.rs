//! XFS, шаг D2: убрать ИМЯ из каталога.
//!
//! Stage 1 у xfs только зануляет данные — файл остаётся в каталоге нулевым
//! призраком. Настоящее удаление (этап D плана, раздел 3.2 в `.ai/PLAN.md`)
//! складывается из четырёх шагов, и это первый: правка самих записей каталога.
//! Инод и его экстенты здесь НЕ освобождаются — это D3 и D4.
//!
//! **Поэтому один D2 не даёт согласованной ФС и в продукт не включён.** После
//! него имя исчезает, а инод остаётся размещённым и ни на что не сослан:
//! `xfs_repair` назовёт его disconnected и предложит перенести в lost+found.
//! Это ожидаемое промежуточное состояние; включать D2 в движок можно только
//! вместе с D3/D4.
//!
//! ## Откуда взялась раскладка
//!
//! Не из головы и не из документации, а из наблюдения за ядром:
//! `scripts/xfs-dir-delta.py` строит образ с каталогами всех четырёх форм,
//! удаляет из каждого по имени ШТАТНЫМ `rm` и печатает расшифрованную разницу.
//! Правило проекта — с чужим форматом сначала проба на настоящих данных.
//!
//! Проба сразу поправила два предположения плана:
//!
//! 1. **Хеш-массив НЕ сдвигается.** План говорил «убрать хеш-запись, сдвинуть».
//!    Ядро вместо этого обнуляет у записи АДРЕС (`XFS_DIR2_NULL_DATAPTR`),
//!    оставляет хеш на месте и увеличивает `stale`; `count` не меняется.
//!    Сдвиг — отдельная ленивая операция уплотнения, и делать её при удалении
//!    не нужно. Это и проще, и меньше поводов ошибиться.
//! 2. **Хвост освобождаемой записи уже правильный.** У записи данных `tag` в
//!    последних двух байтах указывает на её начало — ровно то же требуется от
//!    дыры. Из сорока байт записи ядро меняет четыре: `freetag` и длину.
//!
//! ## Что покрыто и что нет
//!
//! Формы каталога: **shortform** (массив в иноде), **block** (единственный блок
//! с хеш-массивом в хвосте) и **leaf** (блоки данных плюс отдельный лист).
//! Этого хватает на весь общий корпус (`exclusion_corpus`): его плотные
//! каталоги на 300–600 имён — это leaf.
//!
//! Форма **node** (несколько листьев и блоки индекса свободного места)
//! отвергается по имени причины: она появляется на тысячах имён в одном
//! каталоге и требует правки `freeindex`, чего здесь нет.

#![allow(dead_code)] // Шаги D2–D4 завершены и проверены, но в движок не
// подключены: согласованную ФС даёт только вся связка, а решение, какие файлы
// щадить, принимает общий проход D5. До тех пор вызывают их только тесты.

use own_backup_common::{OwnBackupError, Result};

use crate::fat_map::RawVolume;
use crate::xfs_exclusions::{data_fork, fork_extents_at, read_inode, Sb};

fn bad(msg: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::Device(format!("xfs dirent: {msg}"))
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

const FMT_LOCAL: u8 = 1;
const FMT_EXTENTS: u8 = 2;
const FMT_BTREE: u8 = 3;

const DIR3_DATA_MAGIC: u32 = 0x5844_4433; // "XDD3"
const DIR3_BLOCK_MAGIC: u32 = 0x5844_4233; // "XDB3"
const DIR3_FREE_MAGIC: u32 = 0x5844_4633; // "XDF3"
const DIR3_LEAF1_MAGIC: u16 = 0x3DF1;
const DIR3_LEAFN_MAGIC: u16 = 0x3DFF;

/// `xfs_dir3_data_hdr`: 48 байт общего заголовка + три записи bestfree + pad.
const DATA_HDR: usize = 64;
/// Смещение массива bestfree внутри заголовка блока данных.
const BESTFREE_OFF: usize = 0x30;
/// `xfs_da3_blkinfo` — общий заголовок листьев и узлов.
const BLKINFO: usize = 56;
/// `xfs_dir3_leaf_hdr`: blkinfo + count(2) + stale(2) + pad(4).
const LEAF_HDR: usize = 64;
/// CRC лежит по 0x04 в блоках данных и по 0x0C в листьях.
const DATA_CRC_OFF: usize = 4;
const LEAF_CRC_OFF: usize = 12;
/// Свободная запись меньше этого не представима (`freetag`+длина+`tag`).
const MIN_UNUSED: usize = 8;
/// Логическое пространство каталога: листья начинаются с 32 ГиБ, блоки индекса
/// свободного места — с 64 ГиБ. По логическому смещению и отличают их от данных.
const LEAF_OFFSET_BYTES: u64 = 32 * 1024 * 1024 * 1024;
const FREE_OFFSET_BYTES: u64 = 64 * 1024 * 1024 * 1024;
/// «Адреса нет» в хеш-записи листа.
const NULL_DATAPTR: u32 = 0;
/// «Свободного места нет» в массиве bests листа.
const NULL_BEST: u16 = 0xFFFF;

/// Почему имя осталось на месте. Отказ всегда назван — «не смогли» без причины
/// в этом коде считается ошибкой, а не результатом.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DirRefusal {
    /// Форма node: несколько листьев плюс индекс свободного места.
    NodeForm { dir_ino: u64 },
    /// Удаление потребовало бы перекодировать все номера инодов из 8 байт в 4.
    ShortformToIno4 { dir_ino: u64 },
    /// Имени в каталоге нет (уже удалено или каталог не тот).
    NameNotFound { dir_ino: u64, name: String },
    /// Всё остальное — с текстом.
    Other { dir_ino: u64, why: String },
}

impl std::fmt::Display for DirRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DirRefusal::NodeForm { dir_ino } => {
                write!(f, "каталог {dir_ino}: форма node (индекс свободного места) не реализована")
            }
            DirRefusal::ShortformToIno4 { dir_ino } => write!(
                f,
                "каталог {dir_ino}: удаление потребовало бы перекодировки номеров инодов в 4 байта"
            ),
            DirRefusal::NameNotFound { dir_ino, name } => {
                write!(f, "каталог {dir_ino}: имени {name:?} в нём нет")
            }
            DirRefusal::Other { dir_ino, why } => write!(f, "каталог {dir_ino}: {why}"),
        }
    }
}

/// Результат планирования: байтовые патчи и перечень отказов.
#[derive(Debug, Default)]
pub(crate) struct DirEdit {
    /// (физическое смещение, байты) — то же, что у остальных сканеров.
    pub(crate) patches: Vec<(u64, Vec<u8>)>,
    pub(crate) removed: Vec<String>,
    pub(crate) refusals: Vec<DirRefusal>,
}

// --- CRC32C -----------------------------------------------------------------

/// XFS считает CRC32C с затравкой `~0` по блоку, в котором поле CRC обнулено, и
/// пишет дополнение результата в LITTLE-endian.
pub(crate) fn xfs_crc(block: &[u8], crc_off: usize) -> u32 {
    let mut tmp;
    let data = if block[crc_off..crc_off + 4] == [0, 0, 0, 0] {
        block
    } else {
        tmp = block.to_vec();
        tmp[crc_off..crc_off + 4].fill(0);
        &tmp[..]
    };
    !crate::btrfs_exclusions::crc32c_raw(!0u32, data)
}

pub(crate) fn stored_crc(block: &[u8], crc_off: usize) -> u32 {
    u32::from_le_bytes(block[crc_off..crc_off + 4].try_into().unwrap())
}

pub(crate) fn set_crc(block: &mut [u8], crc_off: usize) {
    block[crc_off..crc_off + 4].fill(0);
    let c = xfs_crc(block, crc_off);
    block[crc_off..crc_off + 4].copy_from_slice(&c.to_le_bytes());
}

/// Сверить нашу формулу CRC с тем, что уже записано в блоке.
///
/// Дешёвая, но сильная проверка: если формула неверна или блок повреждён, мы
/// узнаём об этом ДО записи, а не по разрушенной ФС. Тот же приём спас btrfs от
/// молчаливой порчи, когда оттенок crc32c оказался другим.
pub(crate) fn crc_matches(block: &[u8], crc_off: usize) -> bool {
    stored_crc(block, crc_off) == xfs_crc(block, crc_off)
}

// --- разбор блока данных ----------------------------------------------------

/// Одна запись в блоке данных.
#[derive(Debug, Clone)]
struct DataEnt {
    at: usize,
    len: usize,
    name: String,
}

/// Дыра в блоке данных.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Hole {
    at: usize,
    len: usize,
}

/// Разобрать блок данных до `limit` (у block-формы записи кончаются раньше
/// конца блока — дальше хеш-массив и хвост).
fn parse_data(blk: &[u8], sb: &Sb, limit: usize) -> Result<(Vec<DataEnt>, Vec<Hole>)> {
    let mut ents = Vec::new();
    let mut holes = Vec::new();
    let mut p = DATA_HDR;
    while p + MIN_UNUSED <= limit {
        if be16(blk, p) == 0xFFFF {
            let len = be16(blk, p + 2) as usize;
            if len < MIN_UNUSED || p + len > limit {
                return Err(bad(format!("дыра длины {len} по смещению {p} не помещается")));
            }
            holes.push(Hole { at: p, len });
            p += len;
            continue;
        }
        let namelen = blk[p + 8] as usize;
        if namelen == 0 {
            break;
        }
        let raw = 8 + 1 + namelen + usize::from(sb.ftype) + 2;
        let len = (raw + 7) & !7;
        if p + len > limit {
            return Err(bad(format!("запись по смещению {p} выходит за блок")));
        }
        let name = String::from_utf8_lossy(&blk[p + 9..p + 9 + namelen]).to_string();
        ents.push(DataEnt { at: p, len, name });
        p += len;
    }
    Ok((ents, holes))
}

/// Превратить запись в дыру и слить её с соседними дырами.
///
/// Слияние обязательно: два соседних свободных куска, не слитых в один, XFS
/// считает повреждением (`xfs_dir2_data_freescan` строит bestfree по цепочке
/// записей, и висящая пара ломает арифметику свободного места).
fn free_entry(blk: &mut [u8], ent: &DataEnt, holes: &[Hole], limit: usize) -> Result<Hole> {
    let mut start = ent.at;
    let mut end = ent.at + ent.len;
    if let Some(h) = holes.iter().find(|h| h.at + h.len == start) {
        start = h.at;
    }
    if let Some(h) = holes.iter().find(|h| h.at == end) {
        end += h.len;
    }
    let len = end - start;
    if end > limit || len > u16::MAX as usize {
        return Err(bad("слитая дыра не помещается в блок"));
    }
    put_be16(blk, start, 0xFFFF);
    put_be16(blk, start + 2, len as u16);
    // Хвостовой tag дыры указывает на её начало.
    put_be16(blk, end - 2, start as u16);
    Ok(Hole { at: start, len })
}

/// Пересобрать bestfree блока по фактическим дырам — ровно то, что делает
/// `xfs_dir2_data_freescan`: три самые большие, по убыванию длины.
fn rebuild_bestfree(blk: &mut [u8], holes: &[Hole]) -> u16 {
    let mut sorted: Vec<Hole> = holes.iter().copied().filter(|h| h.len >= MIN_UNUSED).collect();
    // По убыванию длины; при равных — по возрастанию смещения, чтобы результат
    // не зависел от порядка обхода.
    sorted.sort_by(|a, b| b.len.cmp(&a.len).then(a.at.cmp(&b.at)));
    for i in 0..3 {
        let o = BESTFREE_OFF + i * 4;
        match sorted.get(i) {
            Some(h) => {
                put_be16(blk, o, h.at as u16);
                put_be16(blk, o + 2, h.len as u16);
            }
            None => {
                put_be16(blk, o, 0);
                put_be16(blk, o + 2, 0);
            }
        }
    }
    sorted.first().map(|h| h.len as u16).unwrap_or(NULL_BEST)
}

/// Адрес записи каталога: `(логический_блок * размер_блока + смещение) / 8`.
///
/// Формула сверена с ядром на живом образе: запись по смещению 800 в четвёртом
/// блоке данных 4 КиБ получила адрес 1636 = (3·4096 + 800)/8.
fn dataptr(sb: &Sb, logical_block: u64, off: usize) -> u32 {
    ((logical_block * sb.blocksize + off as u64) / 8) as u32
}

/// Пометить хеш-запись мёртвой: адрес обнуляется, `stale` растёт, `count` — нет.
fn kill_hash_entry(
    ents_at: usize,
    count: usize,
    stale_at: usize,
    blk: &mut [u8],
    addr: u32,
) -> Result<()> {
    for i in 0..count {
        let o = ents_at + i * 8;
        if be32(blk, o + 4) as u32 == addr {
            put_be32(blk, o + 4, NULL_DATAPTR);
            let stale = be16(blk, stale_at) as u16;
            put_be16(blk, stale_at, stale.saturating_add(1));
            return Ok(());
        }
    }
    Err(bad(format!("в хеш-массиве нет записи с адресом {addr}")))
}

// --- планирование -----------------------------------------------------------

/// Убрать перечисленные имена из каталога `dir_ino`.
///
/// Ничего не пишет: возвращает патчи, которые применит вызывающий. Имена, для
/// которых форма каталога не поддержана, попадают в `refusals` — вместе с
/// причиной, а не молча.
pub(crate) fn plan_dirent_removal(
    vol: &mut dyn RawVolume,
    sb: &Sb,
    dir_ino: u64,
    names: &[String],
) -> Result<DirEdit> {
    let mut edit = DirEdit::default();
    if names.is_empty() {
        return Ok(edit);
    }
    let inode = read_inode(vol, sb, dir_ino)?;
    match inode[5] {
        FMT_LOCAL => shortform_removal(sb, dir_ino, inode, names, &mut edit),
        FMT_EXTENTS | FMT_BTREE => block_removal(vol, sb, dir_ino, &inode, names, &mut edit),
        other => Err(bad(format!("каталог {dir_ino}: формат {other} не поддержан"))),
    }?;
    Ok(edit)
}

/// Массив записей прямо в иноде: сжать его и поправить заголовок.
fn shortform_removal(
    sb: &Sb,
    dir_ino: u64,
    mut inode: Vec<u8>,
    names: &[String],
    edit: &mut DirEdit,
) -> Result<()> {
    let (fs, fe) = data_fork(&inode, sb);
    let sf = &inode[fs..fe];
    if sf.len() < 2 {
        edit.refusals.push(DirRefusal::Other { dir_ino, why: "пустой фок данных".into() });
        return Ok(());
    }
    let count = sf[0] as usize;
    let i8count = sf[1] as usize;
    let ino_sz = if i8count != 0 { 8 } else { 4 };
    let hdr_len = 2 + ino_sz;

    // Разобрать записи: (начало, длина, имя, номер инода).
    let mut ents: Vec<(usize, usize, String, u64)> = Vec::new();
    let mut p = hdr_len;
    for _ in 0..count {
        if p + 3 > sf.len() {
            return Err(bad(format!("каталог {dir_ino}: shortform обрывается")));
        }
        let namelen = sf[p] as usize;
        let name_off = p + 3;
        if name_off + namelen > sf.len() {
            return Err(bad(format!("каталог {dir_ino}: имя выходит за фок")));
        }
        let name = String::from_utf8_lossy(&sf[name_off..name_off + namelen]).to_string();
        let mut q = name_off + namelen;
        if sb.ftype {
            q += 1;
        }
        if q + ino_sz > sf.len() {
            return Err(bad(format!("каталог {dir_ino}: номер инода выходит за фок")));
        }
        let ino = if ino_sz == 8 { be64(sf, q) } else { be32(sf, q) };
        ents.push((p, q + ino_sz - p, name, ino));
        p = q + ino_sz;
    }
    let used = p;

    let wanted: std::collections::HashSet<&String> = names.iter().collect();
    let victims: Vec<&(usize, usize, String, u64)> =
        ents.iter().filter(|(_, _, n, _)| wanted.contains(n)).collect();
    for n in names {
        if !ents.iter().any(|(_, _, name, _)| name == n) {
            edit.refusals.push(DirRefusal::NameNotFound { dir_ino, name: n.clone() });
        }
    }
    if victims.is_empty() {
        return Ok(());
    }

    // Номера, которым нужны 8 байт. Если после удаления таких не остаётся, ядро
    // перекодировало бы ВЕСЬ массив в 4-байтные номера. Мы этого не делаем —
    // отказ дешевле, чем переписывание каждой записи ради нескольких байт.
    if i8count != 0 {
        let big_left = ents
            .iter()
            .filter(|e| !wanted.contains(&e.2))
            .filter(|e| e.3 > u32::MAX as u64)
            .count();
        let parent = be64(sf, 2);
        if big_left == 0 && parent <= u32::MAX as u64 {
            edit.refusals.push(DirRefusal::ShortformToIno4 { dir_ino });
            return Ok(());
        }
    }

    // Собрать оставшиеся записи подряд.
    let mut packed: Vec<u8> = Vec::with_capacity(used - hdr_len);
    let mut removed_big = 0usize;
    for e in &ents {
        if wanted.contains(&e.2) {
            if e.3 > u32::MAX as u64 {
                removed_big += 1;
            }
            continue;
        }
        packed.extend_from_slice(&sf[e.0..e.0 + e.1]);
    }
    let new_used = hdr_len + packed.len();
    let new_count = count - victims.len();

    inode[fs] = u8::try_from(new_count).map_err(|_| bad("count не влезает в байт"))?;
    inode[fs + 1] = u8::try_from(i8count.saturating_sub(removed_big))
        .map_err(|_| bad("i8count не влезает в байт"))?;
    inode[fs + hdr_len..fs + new_used].copy_from_slice(&packed);
    // Хвост за новым концом ядро не чистит, но и мы не обязаны его сохранять:
    // обнулить — честнее, там лежат байты удалённых имён.
    inode[fs + new_used..fs + used].fill(0);

    // di_size — размер фока в байтах.
    put_be64(&mut inode, 0x38, new_used as u64);
    // di_changecount растёт при любом изменении инода.
    let cc = be64(&inode, 0x68);
    put_be64(&mut inode, 0x68, cc.wrapping_add(1));
    set_crc(&mut inode, 0x64);

    edit.patches.push((sb.inode_offset(dir_ino), inode));
    edit.removed.extend(victims.iter().map(|v| v.2.clone()));
    Ok(())
}

/// Формы block и leaf: блоки данных плюс хеш-массив.
fn block_removal(
    vol: &mut dyn RawVolume,
    sb: &Sb,
    dir_ino: u64,
    inode: &[u8],
    names: &[String],
    edit: &mut DirEdit,
) -> Result<()> {
    let nextents = be32(inode, 0x4C);
    let extents = fork_extents_at(vol, sb, inode, inode[5], nextents)?;
    let bs = sb.blocksize as usize;

    let leaf_first = LEAF_OFFSET_BYTES / sb.blocksize;
    let free_first = FREE_OFFSET_BYTES / sb.blocksize;

    // Разложить блоки по ролям: данные, лист, индекс свободного места.
    let mut data_blocks: Vec<(u64, u64)> = Vec::new(); // (логический, физический fsb)
    let mut leaf_blocks: Vec<(u64, u64)> = Vec::new();
    let mut free_blocks = 0usize;
    for (off, e) in &extents {
        for i in 0..e.count {
            let logical = off + i;
            let fsb = e.startblock + i;
            if logical >= free_first {
                free_blocks += 1;
            } else if logical >= leaf_first {
                leaf_blocks.push((logical, fsb));
            } else {
                data_blocks.push((logical, fsb));
            }
        }
    }
    if free_blocks > 0 || leaf_blocks.len() > 1 {
        edit.refusals.push(DirRefusal::NodeForm { dir_ino });
        return Ok(());
    }

    // Прочитать блоки данных и найти жертвы.
    let mut loaded: Vec<(u64, u64, Vec<u8>)> = Vec::new(); // (логический, fsb, байты)
    for (logical, fsb) in &data_blocks {
        let mut blk = vec![0u8; bs];
        vol.read_at(sb.fsblock_to_phys(*fsb), &mut blk)?;
        loaded.push((*logical, *fsb, blk));
    }

    let wanted: std::collections::HashSet<&String> = names.iter().collect();
    let mut found: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Форма block: единственный блок, хеш-массив и хвост в нём же.
    let single_block = loaded.len() == 1 && be32(&loaded[0].2, 0) as u32 == DIR3_BLOCK_MAGIC;
    if !single_block && leaf_blocks.len() != 1 {
        edit.refusals.push(DirRefusal::Other {
            dir_ino,
            why: format!("неожиданная форма: {} блоков данных, {} листьев", loaded.len(), leaf_blocks.len()),
        });
        return Ok(());
    }

    // Лист (для leaf-формы) читаем один раз и правим по мере удаления.
    let mut leaf: Option<(u64, Vec<u8>)> = None;
    if !single_block {
        let (_, fsb) = leaf_blocks[0];
        let mut blk = vec![0u8; bs];
        vol.read_at(sb.fsblock_to_phys(fsb), &mut blk)?;
        let magic = be16(&blk, 8) as u16;
        if magic == DIR3_LEAFN_MAGIC {
            edit.refusals.push(DirRefusal::NodeForm { dir_ino });
            return Ok(());
        }
        if magic != DIR3_LEAF1_MAGIC {
            return Err(bad(format!("каталог {dir_ino}: лист с меткой {magic:#06x}")));
        }
        if !crc_matches(&blk, LEAF_CRC_OFF) {
            return Err(bad(format!("каталог {dir_ino}: CRC листа не сходится — не трогаю")));
        }
        leaf = Some((fsb, blk));
    }

    let mut touched: Vec<usize> = Vec::new();
    for idx in 0..loaded.len() {
        let (logical, _, blk) = &loaded[idx];
        let logical = *logical;
        let magic = be32(blk, 0) as u32;
        if magic == DIR3_FREE_MAGIC {
            continue;
        }
        if magic != DIR3_DATA_MAGIC && magic != DIR3_BLOCK_MAGIC {
            return Err(bad(format!("каталог {dir_ino}: блок с меткой {magic:#010x}")));
        }
        if !crc_matches(blk, DATA_CRC_OFF) {
            return Err(bad(format!("каталог {dir_ino}: CRC блока данных не сходится — не трогаю")));
        }
        // У block-формы записи кончаются перед хеш-массивом.
        let limit = if magic == DIR3_BLOCK_MAGIC {
            let count = be32(blk, bs - 8) as usize;
            bs - 8 - count * 8
        } else {
            bs
        };
        let (ents, holes) = parse_data(blk, sb, limit)?;
        let victims: Vec<DataEnt> =
            ents.into_iter().filter(|e| wanted.contains(&e.name)).collect();
        if victims.is_empty() {
            continue;
        }

        let mut holes = holes;
        let mut blk = loaded[idx].2.clone();
        for v in &victims {
            let addr = dataptr(sb, logical, v.at);
            let merged = free_entry(&mut blk, v, &holes, limit)?;
            holes.retain(|h| !(h.at + h.len == v.at || h.at == v.at + v.len));
            holes.push(merged);
            holes.sort_by_key(|h| h.at);
            found.insert(v.name.clone());

            // Хеш-запись: в самом блоке (block) или в листе (leaf).
            if magic == DIR3_BLOCK_MAGIC {
                let count = be32(&blk, bs - 8) as usize;
                let ents_at = bs - 8 - count * 8;
                kill_hash_entry(ents_at, count, bs - 4 + 2, &mut blk, addr)?;
            } else {
                let (_, lblk) = leaf.as_mut().expect("leaf-форма без листа");
                let count = be16(lblk, BLKINFO) as usize;
                kill_hash_entry(LEAF_HDR, count, BLKINFO + 2, lblk, addr)?;
            }
        }
        let best = rebuild_bestfree(&mut blk, &holes);
        // У leaf-формы лист хранит копию «лучшего свободного» каждого блока.
        if let Some((_, lblk)) = leaf.as_mut() {
            let bestcount = be32(lblk, bs - 4) as usize;
            if (logical as usize) < bestcount {
                let bests_at = bs - 4 - bestcount * 2;
                put_be16(lblk, bests_at + logical as usize * 2, best);
            }
        }
        set_crc(&mut blk, DATA_CRC_OFF);
        loaded[idx].2 = blk;
        touched.push(idx);
    }

    for n in names {
        if !found.contains(n) {
            edit.refusals.push(DirRefusal::NameNotFound { dir_ino, name: n.clone() });
        }
    }
    if found.is_empty() {
        return Ok(());
    }

    for idx in touched {
        let (_, fsb, blk) = &loaded[idx];
        edit.patches.push((sb.fsblock_to_phys(*fsb), blk.clone()));
    }
    if let Some((fsb, mut lblk)) = leaf {
        // У block-формы `stale` живёт в хвосте самого блока, у leaf — в листе;
        // хвост block-формы уже записан выше вместе с блоком.
        set_crc(&mut lblk, LEAF_CRC_OFF);
        edit.patches.push((sb.fsblock_to_phys(fsb), lblk));
    }
    edit.removed.extend(found);
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::xfs_exclusions::parse_sb;
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

    fn have(tool: &str) -> bool {
        std::process::Command::new(tool).arg("-V").output().is_ok()
    }

    fn mkfs(img: &Path, mb: u64) {
        std::fs::File::create(img).unwrap().set_len(mb * 1024 * 1024).unwrap();
        assert!(std::process::Command::new("mkfs.xfs")
            .args(["-q", "-m", "reflink=1,crc=1"])
            .arg(img)
            .status()
            .unwrap()
            .success());
    }

    fn vol(img: &Path) -> FileVol {
        let f = std::fs::File::open(img).unwrap();
        let len = f.metadata().unwrap().len();
        FileVol { f, len }
    }

    fn apply(img: &Path, patches: &[(u64, Vec<u8>)]) {
        let f = std::fs::OpenOptions::new().write(true).open(img).unwrap();
        for (off, bytes) in patches {
            f.write_all_at(bytes, *off).unwrap();
        }
        f.sync_all().unwrap();
    }

    /// Наша формула CRC32C обязана воспроизводить то, что уже записала сама ФС.
    /// Без этой сверки первая же запись ушла бы с неверной суммой, и ядро
    /// отдало бы EIO на чтении — ровно тот класс ошибок, что стоил btrfs
    /// молчаливой порчи.
    #[test]
    fn our_crc_reproduces_what_mkfs_wrote() {
        if !have("mkfs.xfs") {
            eprintln!("нет mkfs.xfs — пропускаю");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("fs.img");
        mkfs(&img, 320);
        let mut v = vol(&img);
        let sb = parse_sb(&mut v).unwrap();

        // Корневой каталог свежей ФС — shortform, так что берём его инод: у
        // инода CRC по 0x64 и считается по тем же правилам.
        let inode = read_inode(&mut v, &sb, sb.rootino).unwrap();
        assert!(
            crc_matches(&inode, 0x64),
            "CRC инода не сошёлся: записано {:#010x}, посчитали {:#010x}",
            stored_crc(&inode, 0x64),
            xfs_crc(&inode, 0x64)
        );

        // Дискриминация: испорченный байт обязан ломать сверку.
        let mut broken = inode.clone();
        broken[200] ^= 0xFF;
        assert!(!crc_matches(&broken, 0x64), "сверка приняла испорченный инод");
    }

    /// bestfree — три самые большие дыры по убыванию длины.
    #[test]
    fn bestfree_keeps_the_three_largest() {
        let mut blk = vec![0u8; 4096];
        let holes = [
            Hole { at: 100, len: 16 },
            Hole { at: 200, len: 64 },
            Hole { at: 300, len: 32 },
            Hole { at: 400, len: 8 },
        ];
        let best = rebuild_bestfree(&mut blk, &holes);
        assert_eq!(best, 64);
        assert_eq!((be16(&blk, 0x30), be16(&blk, 0x32)), (200, 64));
        assert_eq!((be16(&blk, 0x34), be16(&blk, 0x36)), (300, 32));
        assert_eq!((be16(&blk, 0x38), be16(&blk, 0x3A)), (100, 16));

        // Дыр нет — bestfree обнулён, и «лучшего» не существует.
        let empty = rebuild_bestfree(&mut blk, &[]);
        assert_eq!(empty, NULL_BEST);
        assert_eq!(be16(&blk, 0x30), 0);
    }

    /// Соседние дыры обязаны слиться в одну: XFS считает несклеенную пару
    /// повреждением.
    #[test]
    fn freeing_an_entry_merges_with_both_neighbours() {
        let mut blk = vec![0u8; 4096];
        let ent = DataEnt { at: 128, len: 32, name: "x".into() };
        let holes = [Hole { at: 96, len: 32 }, Hole { at: 160, len: 48 }];
        let merged = free_entry(&mut blk, &ent, &holes, 4096).unwrap();
        assert_eq!(merged, Hole { at: 96, len: 32 + 32 + 48 });
        assert_eq!(be16(&blk, 96), 0xFFFF);
        assert_eq!(be16(&blk, 98), 112);
        // Хвостовой tag дыры указывает на её начало.
        assert_eq!(be16(&blk, 96 + 112 - 2), 96);
    }

    /// Адрес записи — из логического номера блока, а не из физического.
    /// Значение сверено с ядром на живом образе (`scripts/xfs-dir-delta.py`).
    #[test]
    fn dataptr_matches_the_kernel() {
        let sb = Sb {
            blocksize: 4096,
            logstart: 0,
            logblocks: 0,
            inodesize: 512,
            agblocks: 0,
            agcount: 0,
            agblklog: 0,
            inopblog: 0,
            rootino: 0,
            sectsize: 512,
            reflink: true,
            ftype: true,
            spinodes: true,
        };
        assert_eq!(dataptr(&sb, 3, 800), 1636);
        assert_eq!(dataptr(&sb, 0, 864), 108);
    }

    /// Хеш-запись помечается мёртвой, а не вырезается: адрес обнуляется,
    /// `stale` растёт, `count` не меняется. Так делает ядро — проверено пробой.
    #[test]
    fn a_dead_hash_entry_keeps_its_slot() {
        let mut blk = vec![0u8; 256];
        let ents_at = 64;
        put_be16(&mut blk, 56, 3); // count
        put_be16(&mut blk, 58, 0); // stale
        for (i, addr) in [10u32, 20, 30].iter().enumerate() {
            put_be32(&mut blk, ents_at + i * 8, 0xAAAA + i as u32);
            put_be32(&mut blk, ents_at + i * 8 + 4, *addr);
        }
        kill_hash_entry(ents_at, 3, 58, &mut blk, 20).unwrap();
        assert_eq!(be16(&blk, 56), 3, "count менять нельзя");
        assert_eq!(be16(&blk, 58), 1, "stale обязан вырасти");
        assert_eq!(be32(&blk, ents_at + 8 + 4), 0, "адрес обнуляется");
        assert_eq!(be32(&blk, ents_at + 8), 0xAAAB, "хеш остаётся на месте");
        assert_eq!(be32(&blk, ents_at + 4), 10, "соседи не двигаются");

        // Дискриминация: чужой адрес — ошибка, а не тихий пропуск.
        assert!(kill_hash_entry(ents_at, 3, 58, &mut blk, 999).is_err());
    }

    /// Приёмка D2 на живой ФС, обе ступени сразу: ядро монтирует и читает, а
    /// `xfs_repair -n` не находит НИЧЕГО про каталоги.
    ///
    /// Единственная жалоба, которая здесь разрешена, — «disconnected inode»:
    /// имя убрано, а инод ещё размещён, потому что освобождают его D3 и D4.
    /// Ровно из-за неё D2 в движок не подключён. Любая другая строка — провал,
    /// и это главное, что тест различает: испорченный хеш-массив, разъехавшийся
    /// bestfree или неверная контрольная сумма выглядят иначе.
    ///
    /// ```sh
    /// cargo test -p own-backup-backup --lib --no-run
    /// sudo <бинарь> --ignored d2_removes_names_the_kernel_agrees_are_gone --nocapture
    /// ```
    #[test]
    #[ignore = "needs root: loop-mounts the patched image and writes to it"]
    fn d2_removes_names_the_kernel_agrees_are_gone() {
        use crate::exclusion_corpus as corpus;
        if !corpus::is_root() {
            eprintln!("не root — пропускаю");
            return;
        }
        for t in ["mkfs.xfs", "xfs_repair"] {
            if !have(t) {
                eprintln!("нет {t} — пропускаю");
                return;
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("fs.img");
        mkfs(&img, 768);
        let mnt = dir.path().join("mnt");
        std::fs::create_dir_all(&mnt).unwrap();

        // Три формы каталога: массив в иноде, один блок, блоки данных с листом.
        // Жертвы — в середине: удаление последней записи умеет схлопываться
        // иначе, и краевой случай не должен изображать общий.
        let plan: [(&str, u32, usize); 3] = [("sf", 3, 1), ("blk", 24, 2), ("leaf", 400, 7)];
        let mut inos: Vec<(String, u64, Vec<String>, Vec<String>)> = Vec::new();
        {
            use std::os::linux::fs::MetadataExt;
            let _m = corpus::mounted(&img, &mnt);
            for (name, n, victims) in plan {
                let d = mnt.join(name);
                std::fs::create_dir(&d).unwrap();
                let mut all = Vec::new();
                for i in 0..n {
                    let f = format!("file-{i:05}.dat");
                    std::fs::write(d.join(&f), b"x".repeat(64)).unwrap();
                    all.push(f);
                }
                // Жертвы вперемешку с соседями, а не подряд.
                let mut gone = Vec::new();
                for k in 0..victims {
                    let f = format!("VICTIM-{k}-long-name-to-remove.dat");
                    std::fs::write(d.join(&f), b"v".repeat(4096)).unwrap();
                    gone.push(f);
                    for i in 0..3u32 {
                        let s = format!("after-{k}-{i:03}.dat");
                        std::fs::write(d.join(&s), b"y".repeat(64)).unwrap();
                        all.push(s);
                    }
                }
                let ino = std::fs::metadata(&d).unwrap().st_ino();
                inos.push((name.to_string(), ino, gone, all));
            }
        }

        // Спланировать и применить.
        let mut v = vol(&img);
        let sb = parse_sb(&mut v).unwrap();
        let mut all_patches = Vec::new();
        for (name, ino, gone, _) in &inos {
            let edit = plan_dirent_removal(&mut v, &sb, *ino, gone).unwrap();
            assert!(
                edit.refusals.is_empty(),
                "каталог {name}: отказы {:?}",
                edit.refusals
            );
            assert_eq!(
                edit.removed.len(),
                gone.len(),
                "каталог {name}: убрано {} из {}",
                edit.removed.len(),
                gone.len()
            );
            assert!(!edit.patches.is_empty(), "каталог {name}: патчей нет");
            eprintln!(
                "  {name}: инод {ino}, убрано {} имён, патчей {}",
                edit.removed.len(),
                edit.patches.len()
            );
            all_patches.extend(edit.patches);
        }
        drop(v);
        apply(&img, &all_patches);

        // Ядро: имена исчезли, соседи целы, запись работает.
        let before = corpus::dmesg_len();
        {
            let _m = corpus::mounted(&img, &mnt);
            for (name, _, gone, kept) in &inos {
                let d = mnt.join(name);
                let listed: std::collections::HashSet<String> = std::fs::read_dir(&d)
                    .unwrap()
                    .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
                    .collect();
                for g in gone {
                    assert!(!listed.contains(g), "каталог {name}: имя {g} осталось");
                    assert!(
                        !d.join(g).exists(),
                        "каталог {name}: {g} всё ещё открывается по имени"
                    );
                }
                for k in kept {
                    assert!(listed.contains(k), "каталог {name}: пропал сосед {k}");
                    let body = std::fs::read(d.join(k)).unwrap();
                    assert!(!body.is_empty(), "каталог {name}: сосед {k} прочитался пустым");
                }
                assert_eq!(
                    listed.len(),
                    kept.len(),
                    "каталог {name}: в списке {} имён вместо {}",
                    listed.len(),
                    kept.len()
                );
                // Каталог должен принимать НОВЫЕ имена: освобождённое место
                // обязано быть пригодным, а не просто помеченным.
                for i in 0..8u32 {
                    std::fs::write(d.join(format!("fresh-{i:03}.dat")), b"z".repeat(200)).unwrap();
                }
            }
            let written = corpus::Corpus::fill(&mnt, 256);
            assert!(written > 16, "записалось всего {written} МиБ");
        }

        // Статика: единственная разрешённая жалоба — оторванный инод.
        let out = std::process::Command::new("xfs_repair").arg("-n").arg(&img).output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let unexpected: Vec<&str> = text
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .filter(|l| {
                // Заголовки фаз и обычный ход работы.
                !l.starts_with("Phase")
                    && !l.starts_with('-')
                    && !l.starts_with("No modify flag set")
                    && !l.starts_with("would ")
                    && !l.contains("disconnected inode")
                    && !l.contains("lost+found")
                    // Шум окружения, а не суждение о нашем образе: под WSL
                    // xfs_repair не может опросить геометрию ФС, на которой
                    // лежит файл образа, и говорит об этом при каждом запуске.
                    && !l.contains("host filesystem geometry")
                    && !l.contains("Repair may fail")
                    && !l.contains("sector size mismatch")
                    && !l.contains("the image and the host filesystem")
            })
            .collect();
        assert!(
            unexpected.is_empty(),
            "xfs_repair пожаловался не только на оторванные иноды:\n{}",
            unexpected.join("\n")
        );
        assert!(
            text.contains("disconnected inode"),
            "ожидали оторванные иноды (их освобождают D3/D4), а их нет:\n{text}"
        );
        let complaints = corpus::kernel_complaints(before, &["XFS", "xfs"]);
        let bad: Vec<&String> =
            complaints.iter().filter(|l| l.contains("error") || l.contains("corrupt")).collect();
        assert!(bad.is_empty(), "ядро ругалось на ФС:\n{bad:?}");
    }

    /// Форма node отвергается по имени причины, а не молча и не порчей.
    #[test]
    #[ignore = "needs root: builds a directory big enough to reach node form"]
    fn node_form_is_refused_by_name() {
        use crate::exclusion_corpus as corpus;
        if !corpus::is_root() || !have("mkfs.xfs") {
            eprintln!("не root или нет mkfs.xfs — пропускаю");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("fs.img");
        mkfs(&img, 768);
        let mnt = dir.path().join("mnt");
        std::fs::create_dir_all(&mnt).unwrap();
        let ino = {
            use std::os::linux::fs::MetadataExt;
            let _m = corpus::mounted(&img, &mnt);
            let d = mnt.join("huge");
            std::fs::create_dir(&d).unwrap();
            for i in 0..6000u32 {
                std::fs::write(d.join(format!("file-{i:06}-with-a-long-name.dat")), b"x").unwrap();
            }
            std::fs::write(d.join("VICTIM.dat"), b"v").unwrap();
            std::fs::metadata(&d).unwrap().st_ino()
        };
        let mut v = vol(&img);
        let sb = parse_sb(&mut v).unwrap();
        let edit = plan_dirent_removal(&mut v, &sb, ino, &["VICTIM.dat".to_string()]).unwrap();
        assert!(edit.patches.is_empty(), "форму node трогать нельзя");
        assert!(edit.removed.is_empty());
        assert!(
            matches!(edit.refusals.as_slice(), [DirRefusal::NodeForm { .. }]),
            "ожидали отказ NodeForm, получили {:?}",
            edit.refusals
        );
    }
}
