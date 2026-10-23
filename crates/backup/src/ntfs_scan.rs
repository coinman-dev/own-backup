//! Этап B2 плана `.ai/exclusions-before-copy-plan_2026-08-19.md`: найти
//! исключаемые файлы, читая $MFT, а не обходя дерево каталогами.
//!
//! Зачем. Обход дерева спрашивает файловую систему про каждую запись по
//! отдельности, и на смонтированном образе это 700 тысяч случайных чтений: на
//! носителе 2026-08-19 такой скан занял 15 мин 59 с, пользователь его отменил.
//! Последовательное чтение MFT того же образа — 14.6 с, в 66 раз быстрее, и
//! находит те же файлы (сверка B0: 99.9%, остальное — дрейф времени между
//! снятием образа и сверкой).
//!
//! И, что важнее скорости: **дескриптор не нужен**. На живом томе обход не смог
//! открыть `pagefile.sys`, `swapfile.sys` и `DumpStack.log.tmp` («файл занят
//! другим процессом») и по политике «не смогли — оставляем» они остались в
//! копии. Ровно те файлы, ради которых исключения и существуют. В MFT они
//! видны как любые другие.
//!
//! Читает и только читает: NTFS-структуры здесь разбираются, но никогда не
//! пишутся. Единственный писатель чужой ФС — драйвер Windows через
//! монтирование (`true_removal_win`), и это правило не меняется.
//!
//! ## ATTRIBUTE_LIST — условие корректности, а не краевой случай
//!
//! Длинный или сильно фрагментированный файл держит свой run list не в базовой
//! записи MFT, а в extension-записях, на которые ссылается ATTRIBUTE_LIST.
//! Читатель, который этого не делает, видит у такого файла НОЛЬ экстентов и
//! молчит. На реальном томе (замер B0) таких записей 6 242 и под ними 17.3 ГиБ;
//! среди исключаемых — 119 файлов на 1079 МиБ, причём 954 МиБ держит ОДИН файл.
//! То есть наивный читатель исключил бы 2000 МиБ вместо 3079 и не сказал бы ни
//! слова, а гигабайт «исключённых» данных уехал бы в копию.
//!
//! Поэтому: за списками атрибутов следуем (это делает крейт `ntfs`), а файл, у
//! которого экстенты не сошлись с его собственным размером, объявляется
//! НЕразрешённым и **остаётся в копии целиком** — исключить часть файла хуже,
//! чем не исключать его вовсе.
//!
//! ## Все потоки `$DATA`, не только безымянный
//!
//! Содержимое файла — это КАЖДЫЙ его атрибут `$DATA`: безымянный и
//! именованные (ADS: `Zone.Identifier`, `com.dropbox.attrs`, что угодно, что
//! программа спрятала рядом). До 2026-10-23 скан брал только безымянный, и
//! кластеры именованных потоков исключённого файла уезжали в копию; на Linux
//! их потом затирал проход удаления, на Windows — никто. Теперь вычитаются
//! run list'ы всех потоков (порванный поток оставляет в копии весь файл, как
//! порванный безымянный), а резидентные — внутри записи MFT — стирает проход
//! удаления у файлов из `whole_records`.
//!
//! Extension-запись — не файл: её атрибуты (части потоков, имена файла с
//! множеством потоков или жёстких ссылок, точка повторного разбора)
//! читаются через ATTRIBUTE_LIST базовой записи, а сама она пропускается.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};

use ntfs::attribute_value::NtfsAttributeValue;
use ntfs::structured_values::{NtfsAttributeList, NtfsFileName, NtfsFileNamespace};
use ntfs::{Ntfs, NtfsAttributeType};
use tracing::{debug, warn};

use own_backup_common::{tr, OwnBackupError, Result};

use crate::exclusions::{parse_patterns, ByteRanges};

/// Корневая запись MFT (`.`) — у неё родитель она сама.
const ROOT_RECORD: u64 = 5;

/// Первые 16 записей MFT — служебные ($MFT, $LogFile, $Bitmap…). Их файлы
/// исключать нельзя ни при каком совпадении маски: без них тома не будет.
const FIRST_USER_RECORD: u64 = 16;

/// `$Extend` — the directory of the volume's later metadata files, whose
/// children have ordinary record numbers.
const EXTEND_RECORD: u64 = 11;

/// Сколько файлов «данные остаются в копии» перечислять в debug поимённо.
/// Остальные попадают только в сводку по причинам. 2026-10-07: 16 400 строк
/// из 16 890 в логе службы были именно такими (распакованные PyInstaller
/// `_MEI*` в %TEMP%), 4.5 МБ на один запуск.
const KEPT_DEBUG_LIMIT: usize = 50;

/// Что нашёл проход по MFT.
#[derive(Default)]
pub struct MftScan {
    /// Экстенты исключаемых файлов — вычитаются из карты занятого ДО копии.
    pub ranges: ByteRanges,
    /// Файлов, чьи экстенты разрешены полностью.
    pub files_excluded: u64,
    /// Байт под ними (выделенные кластеры, не размер файла).
    pub bytes_excluded: u64,
    /// Записей MFT прочитано (живых и мёртвых).
    pub records_scanned: u64,
    /// Записей MFT, до которых не дотянуться: `$MFT` со списком атрибутов
    /// (см. [`mft_reachable_records`]). Их файлы правилами не проверены.
    pub records_unreachable: u64,
    /// Совпавшие — для прохода удаления записей из каталога образа. Как и у
    /// обхода, записываются в ВЕРХНЕЙ точке совпадения: совпавший каталог —
    /// одна запись, всё внутри едет вместе с ним. Иначе список раздувается
    /// (23 519 путей вместо 22 125) и драйвер удаляет по одному то, что мог
    /// снести деревом.
    pub paths: Vec<MatchedPath>,
    /// Совпавшие, чьи данные вычесть НЕЛЬЗЯ, и почему. Они остаются в копии;
    /// запись из каталога уберёт проход удаления.
    pub kept: Vec<(String, KeptReason)>,
    /// Пофайловые байты вычтенного — эталон для сверки с ответом Windows
    /// (`own-backup-cli scan-exclusions --list` пишет такой же).
    pub file_bytes: Vec<(String, u64)>,
    /// Сколько заняли две половины прохода, миллисекунды: чтение и разбор
    /// записей MFT — и вычисление правил. Без этой пары непонятно, что
    /// оптимизировать, а гадать в этом проекте уже выходило боком.
    pub parse_ms: u64,
    pub decide_ms: u64,
    /// Разбивка ОСТАЮЩИХСЯ файлов по типам — для карточки копии. Проход и так
    /// видит каждый файл с его размером, так что это бесплатно; без неё
    /// зашифрованный источник остался бы без разбивки вовсе.
    pub categories: crate::exclusions::CategorySizes,
}

/// Почему данные совпавшего файла остались в копии.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeptReason {
    /// Данные лежат внутри записи MFT — вычитать нечего (кластеров нет).
    Resident,
    /// Экстенты не сошлись с размером файла: часть run list'а не прочитана.
    /// Вычесть то, что нашлось, значит порезать файл наполовину.
    ExtentsIncomplete,
    /// Не все имена файла попадают под исключения (жёсткие ссылки): данные
    /// нужны уцелевшему имени. Урок xfs P3.
    OtherNameKept,
    /// Точка повторного разбора (junction/symlink): цель живёт своей жизнью.
    Reparse,
    /// Запись не разобралась целиком.
    Unreadable,
}

impl KeptReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Resident => "данные внутри записи MFT",
            Self::ExtentsIncomplete => "run list прочитан не полностью",
            Self::OtherNameKept => "у файла есть имя вне исключений (жёсткая ссылка)",
            Self::Reparse => "точка повторного разбора",
            Self::Unreadable => "запись MFT не разобрана",
        }
    }
}

/// Совпавший путь для прохода удаления.
pub struct MatchedPath {
    /// Путь от корня тома, разделитель `\\`.
    pub rel: String,
    pub is_dir: bool,
    /// Junction/symlink: удалять саму точку, никогда не цель.
    pub is_reparse: bool,
    /// MFT records of the files at or under this path every name of which
    /// is excluded (an 8.3 short name is an alias, not a name) and which hold
    /// content inside the record (resident data or a resident named stream),
    /// sorted: the deletion takes the whole file, and only these have their
    /// content cleared first. A record not listed may live on under a name
    /// that stays — or the scan could not tell, or it has nothing resident.
    pub whole_records: Vec<u64>,
}

/// Одно имя файла в MFT.
#[derive(Clone)]
struct Name {
    parent: u64,
    name: String,
}

/// Живая запись MFT, разобранная первым проходом.
#[derive(Clone)]
struct Entry {
    names: Vec<Name>,
    is_dir: bool,
    is_reparse: bool,
    /// Безымянный поток `$DATA` — «содержимое файла» (пуст у каталогов).
    data: Stream,
    /// Именованные потоки `$DATA` (ADS: `Zone.Identifier`,
    /// `com.dropbox.attrs`, что угодно ещё): имя → поток. Они такое же
    /// содержимое файла, как безымянный, и уходят вместе с ним — до
    /// 2026-10-23 скан их пропускал, и кластеры потоков исключённого файла
    /// ехали в копию.
    named: Vec<(Vec<u8>, Stream)>,
    /// Разбор атрибутов упёрся в ошибку — доверять экстентам нельзя.
    broken: bool,
    /// Every `$FILE_NAME` of the record was read: those of its base record
    /// and, through its attribute list, those of its extension records — and
    /// no attribute failed to parse.
    names_complete: bool,
}

/// Один поток `$DATA` записи — все его части, где бы они ни лежали (длинный
/// run list NTFS режет на несколько экземпляров атрибута, в том числе в
/// разных extension-записях).
#[derive(Clone, Default, Debug)]
struct Stream {
    /// Размер данных потока (у экземпляра со StartVCN=0; у остальных 0).
    size: u64,
    /// (смещение на томе, длина) выделенных кластеров.
    extents: Vec<(u64, u64)>,
    /// Сумма длин экстентов — насколько ФС выделила места под поток.
    allocated: u64,
    /// Сжат или разрежен по флагам АТРИБУТА, а не по догадке. У таких
    /// потоков выделено законно меньше, чем они весят, и путать это с
    /// недочитанным run list'ом нельзя — см. `decide`.
    compressed_or_sparse: bool,
    /// Данные внутри записи MFT: кластеров нет, вычитать нечего.
    resident: bool,
}

impl Stream {
    /// Экстенты не сходятся с размером: часть run list'а не прочитана (за
    /// ATTRIBUTE_LIST или битая запись). Вычесть найденное — значит порезать
    /// поток наполовину; молчать тут нельзя.
    fn incomplete(&self, cluster_size: u64) -> bool {
        if self.resident {
            return false;
        }
        (self.size > 0 && self.allocated == 0)
            || (self.allocated < round_up(self.size, cluster_size) && !self.compressed_or_sparse)
    }
}

/// Поток `name` записи (сырые байты UTF-16LE имени; пустое — безымянный),
/// созданный при первой встрече.
fn stream_mut<'a>(
    data: &'a mut Stream,
    named: &'a mut Vec<(Vec<u8>, Stream)>,
    name: &[u8],
) -> &'a mut Stream {
    if name.is_empty() {
        return data;
    }
    let i = match named.iter().position(|(n, _)| n == name) {
        Some(i) => i,
        None => {
            named.push((name.to_vec(), Stream::default()));
            named.len() - 1
        }
    };
    &mut named[i].1
}

/// Добавить к потоку `s` одну часть атрибута `$DATA` (резидентную или с run
/// list'ом). `false` — значение не прочиталось.
fn take_data_part<T>(
    attr: &ntfs::NtfsAttribute<'_, '_>,
    fs: &mut T,
    s: &mut Stream,
    record: u64,
) -> bool
where
    T: Read + Seek,
{
    use ntfs::NtfsAttributeFlags as F;
    let f = attr.flags();
    if f.contains(F::COMPRESSED) || f.contains(F::SPARSE) {
        s.compressed_or_sparse = true;
    }
    match attr.value(fs) {
        Ok(NtfsAttributeValue::Resident(v)) => {
            s.resident = true;
            s.size = s.size.max(v.len());
            true
        }
        Ok(NtfsAttributeValue::NonResident(v)) => {
            // Длинный run list NTFS кладёт в НЕСКОЛЬКО экземпляров $DATA, и
            // размер заполнен только у первого (StartVCN=0) — экстенты
            // накапливаем, размер берём максимальный (проба B0 на этом
            // спотыкалась и теряла крупные файлы).
            s.size = s.size.max(v.len());
            let mut broken = false;
            take_runs(
                v.data_runs(),
                &mut s.extents,
                &mut s.allocated,
                &mut broken,
                record,
            );
            !broken
        }
        Ok(NtfsAttributeValue::AttributeListNonResident(v)) => {
            s.size = s.size.max(v.len());
            true
        }
        Err(e) => {
            warn!(record, error = %e, "MFT scan: значение $DATA не прочитано");
            false
        }
    }
}

/// Имя атрибута `$DATA` (пустое у безымянного потока) — сырые байты
/// UTF-16LE: два имени, которые `to_string_lossy` свёл бы в одно (непарные
/// суррогаты), остаются двумя потоками.
fn attr_name(attr: &ntfs::NtfsAttribute<'_, '_>) -> Option<Vec<u8>> {
    if attr.name_length() == 0 {
        return Some(Vec::new());
    }
    attr.name().ok().map(|n| n.0.to_vec())
}

/// Offset of the base file reference in an MFT record header: zero in a base
/// record, the record it belongs to in an extension record.
const BASE_REFERENCE_OFFSET: u64 = 0x20;

/// The low 48 bits of a file reference are the record number, the high 16
/// its sequence number.
const RECORD_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

/// The file reference (record number and sequence number) of the base record
/// `file` extends, or 0 when `file` is a base record itself. Read from the
/// record header directly: the `ntfs` crate keeps the field private. `None`
/// when it cannot be read.
fn base_reference_of<T>(fs: &mut T, file: &ntfs::NtfsFile<'_>) -> Option<u64>
where
    T: Read + Seek,
{
    let at = file.position().value()?.get();
    let mut raw = [0u8; 8];
    fs.seek(SeekFrom::Start(at + BASE_REFERENCE_OFFSET)).ok()?;
    fs.read_exact(&mut raw).ok()?;
    Some(u64::from_le_bytes(raw))
}

/// Пройти MFT тома `fs` и собрать всё, что подпадает под `patterns`.
///
/// `fs` — том целиком (смещение 0 = загрузочный сектор). Читается
/// последовательно по записям MFT; ни одного открытия файла.
pub fn scan_mft<T>(fs: &mut T, patterns: &[String]) -> Result<MftScan>
where
    T: Read + Seek,
{
    scan_mft_with(fs, patterns, |_, _| {})
}

/// То же, но с докладом о ходе: `tick(прочитано, всего)`.
///
/// Без него проход молчит две с лишним минуты на томе в 128 ГиБ (замер на
/// носителе 2026-08-19: 1 079 808 записей за 2 мин 17 с), и человек за экраном
/// видит пустую полосу и решает, что программа повисла. Так и было.
pub fn scan_mft_with<T>(
    fs: &mut T,
    patterns: &[String],
    mut tick: impl FnMut(u64, u64),
) -> Result<MftScan>
where
    T: Read + Seek,
{
    scan_mft_with_cancel(fs, patterns, |done, total| {
        tick(done, total);
        Ok(())
    })
}

/// Scan with a fallible progress callback. Cancellation is returned unchanged
/// before reading the next MFT record; callers must not retry it as a scan error.
pub fn scan_mft_with_cancel<T>(
    fs: &mut T,
    patterns: &[String],
    mut tick: impl FnMut(u64, u64) -> Result<()>,
) -> Result<MftScan>
where
    T: Read + Seek,
{
    let started = std::time::Instant::now();
    let parsed = parse_patterns(patterns);
    let mut scan = MftScan::default();
    if parsed.is_empty() {
        scan.ranges.finalize();
        return Ok(scan);
    }

    let mut ntfs = Ntfs::new(fs).map_err(|e| bad(tr!("ntfs_scan.not_ntfs", error = e)))?;
    ntfs.read_upcase_table(fs)
        .map_err(|e| bad(tr!("ntfs_scan.upcase_table", error = e)))?;
    let cluster_size = ntfs.cluster_size() as u64;
    let record_size = ntfs.file_record_size() as u64;

    // Грязный том — отказ, как у ext (P4), btrfs (P5) и xfs (E6). Метаданные
    // такого тома описывают состояние ДО проигрывания журнала: экстенты, снятые
    // с них, могут указывать на кластеры, которые файловая система уже считает
    // чужими, и вычитание вырежет из копии живые данные. Вызывающий откатится
    // на проход внутри образа — там журнал проигрывает сам драйвер Windows, в
    // копии, не трогая источник.
    if let Ok(info) = ntfs.volume_info(fs) {
        if info
            .flags()
            .contains(ntfs::structured_values::NtfsVolumeFlags::IS_DIRTY)
        {
            return Err(bad(tr!("ntfs_scan.volume_dirty")));
        }
    }

    // Сколько всего записей: длина данных самой $MFT, делённая на размер записи.
    let mft = ntfs
        .file(fs, 0)
        .map_err(|e| bad(tr!("ntfs_scan.mft_record_zero", error = e)))?;
    let mft_len = data_len(&mft, fs).ok_or_else(|| bad(tr!("ntfs_scan.mft_no_data")))?;
    let total = mft_len / record_size;
    debug!(total, record_size, cluster_size, "MFT scan: начинаю");
    // A `$MFT` so fragmented that its own run list spilled into extension
    // records: the `ntfs` crate finds a record only through the first `$DATA`
    // of record 0, and the records past it cannot be read. Their files are
    // not seen by the rules at all — counted and said, not skipped in silence.
    let reachable = match mft_reachable_records(&mft, fs, record_size) {
        Some(limit) if limit < total => {
            scan.records_unreachable = total - limit;
            warn!(
                reachable = limit,
                unreachable = total - limit,
                "MFT scan: $MFT has an attribute list — the records past its first run \
                 list cannot be read, and their files are NOT checked by the rules"
            );
            limit
        }
        _ => total,
    };

    // --- проход 1: разобрать записи -----------------------------------------
    let mut entries: HashMap<u64, Entry> = HashMap::with_capacity(total as usize / 2);
    for n in 0..reachable {
        scan.records_scanned += 1;
        // Раз в 4096 записей: на миллионе это 260 сообщений, а не миллион.
        if scan.records_scanned % 4096 == 0 {
            tick(scan.records_scanned, reachable)?;
        }
        let file = match ntfs.file(fs, n) {
            Ok(f) => f,
            // Мёртвая или неполная запись — таких в MFT штатно много.
            Err(_) => continue,
        };
        if !file.flags().contains(ntfs::NtfsFileFlags::IN_USE) {
            continue;
        }
        // Extension-запись — продолжение чужой базовой: её атрибуты (имена,
        // части потоков) читаются через ATTRIBUTE_LIST базовой записи ниже.
        // Сама по себе она не файл: имя в ней — имя базовой записи.
        match base_reference_of(fs, &file) {
            Some(r) if r & RECORD_MASK == 0 => {}
            Some(_) => continue,
            None => {
                warn!(record = n, "MFT scan: заголовок записи не прочитан");
                continue;
            }
        }
        if let Some(entry) = parse_record(&ntfs, fs, &file, n) {
            entries.insert(n, entry);
        }
    }
    debug!(live = entries.len(), "MFT scan: записи разобраны");
    scan.parse_ms = started.elapsed().as_millis() as u64;
    let pass2 = std::time::Instant::now();

    // --- проход 2: пути, совпадения, вычитание -------------------------------
    // Вынесен в отдельную функцию намеренно: здесь живут ПРАВИЛА (что вычесть,
    // что оставить, что записать в удаление), и они не про NTFS. Разбор NTFS
    // проверен байт-в-байт против самой Windows на настоящем томе, а правила
    // проверяются юнит-тестами внизу файла — на записях, собранных руками.
    // Синтетическая NTFS-фикстура тут была бы самообманом: она испытывала бы
    // наш парсер против нашего же писателя (урок BitLocker, четыре захода).
    decide(&entries, &parsed, cluster_size, &mut scan);
    scan.ranges.finalize();
    scan.decide_ms = pass2.elapsed().as_millis() as u64;
    debug!(
        parse_ms = scan.parse_ms,
        decide_ms = scan.decide_ms,
        "MFT scan: готово"
    );
    Ok(scan)
}

/// Правила: что вычесть из копии, что оставить, что записать на удаление.
///
/// Отдельно от чтения NTFS, потому что это разные вещи и ошибаются они
/// по-разному. Чтение проверено сверкой с Windows; правила — тестами ниже.
fn decide(
    entries: &HashMap<u64, Entry>,
    parsed: &[crate::exclusions::ExclusionPattern],
    cluster_size: u64,
    scan: &mut MftScan,
) {
    // --- проход 2: пути, совпадения, вычитание -------------------------------
    let mut path_cache: HashMap<u64, Option<Vec<String>>> = HashMap::new();
    path_cache.insert(ROOT_RECORD, Some(Vec::new()));

    // Путь записи: имена родителей до корня. `None` — цепочка оборвалась.
    fn path_of(
        n: u64,
        entries: &HashMap<u64, Entry>,
        cache: &mut HashMap<u64, Option<Vec<String>>>,
        depth: u32,
    ) -> Option<Vec<String>> {
        if let Some(hit) = cache.get(&n) {
            return hit.clone();
        }
        if depth > 128 {
            return None;
        }
        let entry = entries.get(&n)?;
        let name = entry.names.first()?;
        if name.parent == n {
            return None;
        }
        let up = path_of(name.parent, entries, cache, depth + 1);
        let out = up.map(|mut v| {
            v.push(name.name.clone());
            v
        });
        cache.insert(n, out.clone());
        out
    }

    let matches = |comps: &[String]| -> bool {
        let Some(last) = comps.last() else {
            return false;
        };
        let lower: Vec<String> = comps.iter().map(|c| c.to_lowercase()).collect();
        let name_lower = last.to_lowercase();
        parsed.iter().any(|p| p.matches(&name_lower, &lower))
    };

    // Каталоги, попавшие под правила: всё под ними исключается вместе с ними.
    let mut excluded_dirs: std::collections::HashSet<u64> = std::collections::HashSet::new();
    for (&n, e) in entries {
        if !e.is_dir || is_system(n, entries) {
            continue;
        }
        if let Some(p) = path_of(n, &entries, &mut path_cache, 0) {
            if !p.is_empty() && matches(&p) {
                excluded_dirs.insert(n);
            }
        }
    }

    let mut unresolved = 0u64;
    // Which recorded path each top match point is, and the files whose whole
    // record goes with the entry of their top match point.
    let mut path_index: HashMap<u64, usize> = HashMap::new();
    let mut whole: Vec<(u64, u64)> = Vec::new();
    for (&n, e) in entries {
        if is_system(n, entries) {
            continue; // служебные файлы тома неприкосновенны
        }
        let Some(p) = path_of(n, &entries, &mut path_cache, 0) else {
            unresolved += 1;
            continue;
        };
        if p.is_empty() {
            continue;
        }
        // Совпало само имя — или что-то выше по дереву.
        let inherited = under_excluded(n, &entries, &excluded_dirs);
        let hit = matches(&p) || inherited;
        if !hit {
            if !e.is_dir {
                let lower: Vec<String> = p.iter().map(|c| c.to_lowercase()).collect();
                scan.categories.add(&lower, e.data.size);
            }
            continue;
        }
        let rel = p.join("\\");
        // Верхняя точка совпадения: то, что попало под исключение только
        // потому, что лежит внутри исключённого каталога, отдельной записи не
        // получает — каталог унесёт его с собой.
        if !inherited {
            path_index.insert(n, scan.paths.len());
            scan.paths.push(MatchedPath {
                rel: rel.clone(),
                is_dir: e.is_dir,
                is_reparse: e.is_reparse,
                whole_records: Vec::new(),
            });
        }
        if e.is_dir {
            // A directory goes as a whole (it has no hard links), and its
            // named streams with it — Dropbox, for one, tags directories with
            // `com.dropbox.attrs`. The folder Windows recreates on mount stays,
            // and so do its streams.
            let named_ok = e.named.iter().all(|(_, s)| !s.incomplete(cluster_size));
            if !e.broken
                && !e.is_reparse
                && named_ok
                && !crate::exclusions::recreated_by_windows(&rel)
            {
                scan.bytes_excluded += subtract_named(e, &mut scan.ranges);
            }
            continue;
        }
        let rel_for_bytes = rel.clone();
        let every_name_excluded = e.names.len() == 1
            || all_names_excluded(e, entries, &mut path_cache, &matches, &excluded_dirs);
        // Only a record that holds content of its own — resident data or a
        // resident named stream — has anything for the deletion to clear;
        // the rest is in clusters the copy leaves out. A Temp folder of
        // twenty thousand files is not opened twice for nothing.
        let holds_content = e.data.resident || e.named.iter().any(|(_, s)| s.resident);
        if every_name_excluded && e.names_complete && holds_content {
            let top = if inherited {
                top_excluded_dir(n, entries, &excluded_dirs)
            } else {
                Some(n)
            };
            if let Some(top) = top {
                whole.push((top, n));
            }
        }

        // Every named stream's run list read in full: a stream cut in half is
        // as bad as a file cut in half.
        let named_ok = e.named.iter().all(|(_, s)| !s.incomplete(cluster_size));
        // Данные остаются в копии, если...
        let reason = if e.broken {
            Some(KeptReason::Unreadable)
        } else if e.is_reparse {
            Some(KeptReason::Reparse)
        } else if e.data.resident {
            Some(KeptReason::Resident)
        } else if !every_name_excluded {
            Some(KeptReason::OtherNameKept)
        } else if e.data.incomplete(cluster_size) || !named_ok {
            // Файл не пуст, а экстентов нет или их меньше размера: run list
            // не прочитан (за ATTRIBUTE_LIST или битая запись). Молчать тут
            // нельзя.
            Some(KeptReason::ExtentsIncomplete)
        } else {
            None
        };

        match reason {
            Some(r) => {
                // Пофайлово — в debug, и только первые KEPT_DEBUG_LIMIT: на томе с
                // забитым %TEMP% таких десятки тысяч, и лог службы раздувается до
                // мегабайт. Наружу идёт одна сводка ниже.
                if scan.kept.len() < KEPT_DEBUG_LIMIT {
                    debug!(file = %rel, reason = r.as_str(), "MFT scan: данные остаются в копии");
                }
                // A small file keeps its content inside its record, but its
                // named streams may well be in clusters of their own — those
                // leave the copy like any other excluded data. The resident
                // part is cleared by the deletion pass (`whole_records`).
                if r == KeptReason::Resident && every_name_excluded && named_ok {
                    let bytes = subtract_named(e, &mut scan.ranges);
                    if bytes > 0 {
                        scan.bytes_excluded += bytes;
                        scan.file_bytes.push((rel_for_bytes, bytes));
                    }
                }
                scan.kept.push((rel, r));
            }
            None => {
                for (off, len) in &e.data.extents {
                    scan.ranges.add(*off, off + len);
                }
                let bytes = e.data.allocated + subtract_named(e, &mut scan.ranges);
                scan.files_excluded += 1;
                scan.bytes_excluded += bytes;
                scan.file_bytes.push((rel_for_bytes, bytes));
            }
        }
    }
    for (top, n) in whole {
        if let Some(&i) = path_index.get(&top) {
            scan.paths[i].whole_records.push(n);
        }
    }
    for p in &mut scan.paths {
        p.whole_records.sort_unstable();
    }
    if unresolved > 0 {
        warn!(
            records = unresolved,
            "MFT scan: путь не построен — эти записи правилами НЕ проверялись"
        );
    }
    if !scan.kept.is_empty() {
        // Сводка по причинам: сколько совпавших файлов остаются в копии данными
        // (запись из каталога у них всё равно удаляется). Подробности — в debug.
        let mut by_reason: std::collections::BTreeMap<&str, usize> = Default::default();
        for (_, r) in &scan.kept {
            *by_reason.entry(r.as_str()).or_default() += 1;
        }
        let summary = by_reason
            .iter()
            .map(|(r, n)| format!("{r}: {n}"))
            .collect::<Vec<_>>()
            .join(", ");
        warn!(
            files = scan.kept.len(),
            reasons = %summary,
            "MFT scan: у части совпавших файлов данные остаются в копии"
        );
        if scan.kept.len() > KEPT_DEBUG_LIMIT {
            debug!(
                listed = KEPT_DEBUG_LIMIT,
                total = scan.kept.len(),
                "MFT scan: поимённо перечислены не все такие файлы, остальные только в сводке"
            );
        }
    }
}

/// A file of the volume itself: one of the first records ($MFT, $LogFile,
/// $Bitmap…), or anything in the `$Extend` tree — `$UsnJrnl` and its `$J`
/// stream, `$Quota`, `$ObjId`, `$Reparse`, `$RmMetadata` with `$Repair` and
/// `$TxfLog`. Those have record numbers like any user file, and a rule that
/// matched one (`$Extend\$UsnJrnl`, `**\$J`…) would cut the volume's own
/// metadata out of the copy. Decided by the parent chain of every name, not by
/// the path text.
fn is_system(n: u64, entries: &HashMap<u64, Entry>) -> bool {
    if n < FIRST_USER_RECORD {
        return true;
    }
    let Some(e) = entries.get(&n) else {
        return false;
    };
    e.names.iter().any(|name| {
        let mut parent = name.parent;
        for _ in 0..128 {
            if parent == EXTEND_RECORD {
                return true;
            }
            if parent == ROOT_RECORD {
                return false;
            }
            match entries.get(&parent).and_then(|p| p.names.first()) {
                Some(up) if up.parent != parent => parent = up.parent,
                _ => return false,
            }
        }
        false
    })
}

fn under_excluded(
    n: u64,
    entries: &HashMap<u64, Entry>,
    excluded: &std::collections::HashSet<u64>,
) -> bool {
    let mut cur = n;
    for _ in 0..128 {
        let Some(e) = entries.get(&cur) else {
            return false;
        };
        let Some(name) = e.names.first() else {
            return false;
        };
        if name.parent == cur || name.parent == ROOT_RECORD {
            return false;
        }
        if excluded.contains(&name.parent) {
            return true;
        }
        cur = name.parent;
    }
    false
}

/// The highest excluded directory above `n` along its first name: the top
/// match point an entry inside an excluded directory is recorded under.
fn top_excluded_dir(
    n: u64,
    entries: &HashMap<u64, Entry>,
    excluded: &std::collections::HashSet<u64>,
) -> Option<u64> {
    let mut top = None;
    let mut cur = n;
    for _ in 0..128 {
        let name = entries.get(&cur)?.names.first()?;
        if name.parent == cur || name.parent == ROOT_RECORD {
            return top;
        }
        if excluded.contains(&name.parent) {
            top = Some(name.parent);
        }
        cur = name.parent;
    }
    None
}

/// Все ли имена файла попадают под исключения. Данные общие: пока живо хоть
/// одно имя, вычитать их нельзя — иначе уцелевшее имя укажет на пустоту.
fn all_names_excluded(
    e: &Entry,
    entries: &HashMap<u64, Entry>,
    cache: &mut HashMap<u64, Option<Vec<String>>>,
    matches: &impl Fn(&[String]) -> bool,
    excluded: &std::collections::HashSet<u64>,
) -> bool {
    for name in &e.names {
        let Some(mut path) = path_of_pub(name.parent, entries, cache, 0) else {
            return false;
        };
        path.push(name.name.clone());
        // The name itself, or any directory above it, under the rules — as
        // `under_excluded` sees a file, and the Windows walk a hard link
        // (`exclusions::every_name_excluded`); not only the parent.
        let under_rules =
            excluded.contains(&name.parent) || (1..=path.len()).any(|k| matches(&path[..k]));
        if !under_rules {
            return false;
        }
    }
    true
}

/// The path of record `n` from the root, along its first name; `None` when
/// the chain breaks, loops or runs deeper than any real tree (128) — a
/// corrupt volume must not overflow the service's stack.
fn path_of_pub(
    n: u64,
    entries: &HashMap<u64, Entry>,
    cache: &mut HashMap<u64, Option<Vec<String>>>,
    depth: u32,
) -> Option<Vec<String>> {
    if n == ROOT_RECORD {
        return Some(Vec::new());
    }
    if let Some(hit) = cache.get(&n) {
        return hit.clone();
    }
    if depth > 128 {
        return None;
    }
    let entry = entries.get(&n)?;
    let name = entry.names.first()?;
    if name.parent == n {
        return None;
    }
    let up = path_of_pub(name.parent, entries, cache, depth + 1);
    let out = up.map(|mut v| {
        v.push(name.name.clone());
        v
    });
    cache.insert(n, out.clone());
    out
}

/// Leave the clusters of every named stream of `e` out of the copy. Returns
/// the bytes they occupy.
fn subtract_named(e: &Entry, ranges: &mut ByteRanges) -> u64 {
    let mut bytes = 0u64;
    for (_, s) in &e.named {
        for (off, len) in &s.extents {
            ranges.add(*off, off + len);
        }
        bytes += s.allocated;
    }
    bytes
}

/// How many records of the MFT the `ntfs` crate can reach, when `$MFT`
/// itself has an attribute list: `Ntfs::file` seeks through the first `$DATA`
/// of record 0 only, so the records past that attribute's run list cannot be
/// read. `None` when there is no attribute list (every record is reachable).
fn mft_reachable_records<T>(mft: &ntfs::NtfsFile<'_>, fs: &mut T, record_size: u64) -> Option<u64>
where
    T: Read + Seek,
{
    let mut has_list = false;
    let mut first_data = None;
    for attr in mft.attributes_raw() {
        let attr = attr.ok()?;
        match attr.ty() {
            Ok(NtfsAttributeType::AttributeList) => has_list = true,
            Ok(NtfsAttributeType::Data) if attr.name_length() == 0 && first_data.is_none() => {
                first_data = Some(attr)
            }
            _ => {}
        }
    }
    if !has_list {
        return None;
    }
    let mut covered = 0u64;
    if let Some(Ok(NtfsAttributeValue::NonResident(v))) = first_data.map(|a| a.value(fs)) {
        for run in v.data_runs().flatten() {
            covered += run.allocated_size();
        }
    }
    Some(covered / record_size.max(1))
}

fn round_up(v: u64, to: u64) -> u64 {
    if to == 0 {
        return v;
    }
    v.div_ceil(to) * to
}

fn data_len<T>(file: &ntfs::NtfsFile<'_>, fs: &mut T) -> Option<u64>
where
    T: Read + Seek,
{
    let mut attrs = file.attributes();
    while let Some(item) = attrs.next(fs) {
        let item = item.ok()?;
        let attr = item.to_attribute().ok()?;
        if attr.ty().ok()? == NtfsAttributeType::Data && attr.name_length() == 0 {
            return Some(attr.value_length());
        }
    }
    None
}

/// Забрать экстенты из итератора data runs. Дыра разреженного файла места на
/// диске не занимает, вычитать её нечего.
fn take_runs(
    runs: ntfs::attribute_value::NtfsDataRuns<'_, '_>,
    extents: &mut Vec<(u64, u64)>,
    allocated: &mut u64,
    broken: &mut bool,
    record: u64,
) {
    for run in runs {
        match run {
            Ok(r) => {
                if let Some(pos) = r.data_position().value() {
                    extents.push((pos.get(), r.allocated_size()));
                    *allocated += r.allocated_size();
                }
            }
            Err(e) => {
                warn!(record, error = %e, "MFT scan: run list не разобран");
                *broken = true;
            }
        }
    }
}

/// Разобрать живую базовую запись `n`: имена, флаги, все потоки `$DATA` —
/// безымянный и именованные, резидентные и с run list'ом, в самой записи и в
/// её extension-записях. `None` — у записи нет имени (служебная без имени,
/// осиротевшая).
fn parse_record<T>(ntfs: &Ntfs, fs: &mut T, file: &ntfs::NtfsFile<'_>, n: u64) -> Option<Entry>
where
    T: Read + Seek,
{
    let mut names = Vec::new();
    let mut data = Stream::default();
    let mut named: Vec<(Vec<u8>, Stream)> = Vec::new();
    let mut broken = false;
    let mut is_reparse = false;

    // Атрибуты базовой записи. Плоский вид крейта (`attributes()`) сам ходит
    // по ATTRIBUTE_LIST, но отдаёт связанные данные ПОТОКОМ, без списка
    // экстентов — а нам нужны именно экстенты. Поэтому базовую запись читаем
    // сырым итератором, а за списком идём сами (ниже).
    let mut attr_list_present = false;
    for attr in file.attributes_raw() {
        let attr = match attr {
            Ok(a) => a,
            Err(e) => {
                warn!(record = n, error = %e, "MFT scan: атрибут не разобран");
                broken = true;
                break;
            }
        };
        match attr.ty() {
            Ok(NtfsAttributeType::FileName) => match attr.structured_value::<_, NtfsFileName>(fs) {
                Ok(fname) => push_name(&mut names, &fname),
                Err(_) => broken = true,
            },
            Ok(NtfsAttributeType::ReparsePoint) => is_reparse = true,
            Ok(NtfsAttributeType::AttributeList) => attr_list_present = true,
            Ok(NtfsAttributeType::Data) => {
                // Все потоки, не только безымянный: именованный (ADS) —
                // такое же содержимое файла и уходит вместе с ним.
                let Some(name) = attr_name(&attr) else {
                    broken = true;
                    continue;
                };
                let s = stream_mut(&mut data, &mut named, &name);
                if !take_data_part(&attr, fs, s, n) {
                    broken = true;
                }
            }
            _ => {}
        }
    }

    // ATTRIBUTE_LIST: части потоков, имена и прочее лежат в других записях
    // MFT. Без этого прохода у крупного фрагментированного файла экстентов
    // ноль — на реальном томе так пряталось 954 МиБ в одном файле (замер B0);
    // у файла с множеством потоков или жёстких ссылок там же оказываются и
    // его имена.
    if attr_list_present {
        let mut parts = ListParts {
            names: &mut names,
            data: &mut data,
            named: &mut named,
            is_reparse: &mut is_reparse,
        };
        if let Err(e) = read_attribute_list(ntfs, fs, file, &mut parts) {
            warn!(record = n, error = %e, "MFT scan: список атрибутов не пройден");
            broken = true;
        }
    }

    if names.is_empty() {
        return None; // запись без имени
    }
    Some(Entry {
        names,
        is_dir: file.is_directory(),
        is_reparse,
        data,
        named,
        broken,
        names_complete: !broken,
    })
}

/// Короткое имя 8.3 — псевдоним, а не отдельная ссылка.
fn push_name(names: &mut Vec<Name>, fname: &NtfsFileName) {
    if fname.namespace() != NtfsFileNamespace::Dos {
        names.push(Name {
            parent: fname.parent_directory_reference().file_record_number(),
            name: fname.name().to_string_lossy(),
        });
    }
}

/// Куда [`read_attribute_list`] складывает найденное в extension-записях.
struct ListParts<'a> {
    names: &'a mut Vec<Name>,
    data: &'a mut Stream,
    named: &'a mut Vec<(Vec<u8>, Stream)>,
    is_reparse: &'a mut bool,
}

/// Пройти ATTRIBUTE_LIST записи `file` и забрать из её extension-записей всё,
/// что решает судьбу файла: части КАЖДОГО потока `$DATA` (безымянного и
/// именованных), имена `$FILE_NAME` и признак точки повторного разбора.
/// Записи MFT находятся по номерам через run list самой `$MFT` (крейт), а не
/// по догадке о смежности. Ошибка — записи не доверяем целиком.
fn read_attribute_list<T>(
    ntfs: &Ntfs,
    fs: &mut T,
    file: &ntfs::NtfsFile<'_>,
    parts: &mut ListParts<'_>,
) -> Result<()>
where
    T: Read + Seek,
{
    let base = file.file_record_number();
    let mut list_attr = None;
    for attr in file.attributes_raw() {
        let attr = attr.map_err(|e| bad(tr!("ntfs_scan.attribute", error = e)))?;
        if attr.ty().ok() == Some(NtfsAttributeType::AttributeList) {
            list_attr = Some(attr);
            break;
        }
    }
    let Some(attr) = list_attr else {
        return Ok(());
    };
    let list = attr
        .structured_value::<_, NtfsAttributeList<'_, '_>>(fs)
        .map_err(|e| bad(tr!("ntfs_scan.attribute_list", error = e)))?;

    let mut entries = list.entries();
    while let Some(entry) = entries.next(fs) {
        let entry = entry.map_err(|e| bad(tr!("ntfs_scan.list_entry", error = e)))?;
        // Атрибуты, лежащие в самой базовой записи, уже прочитаны выше.
        if entry.base_file_reference().file_record_number() == base {
            continue;
        }
        let ty = match entry.ty() {
            Ok(
                t @ (NtfsAttributeType::Data
                | NtfsAttributeType::FileName
                | NtfsAttributeType::ReparsePoint),
            ) => t,
            _ => continue,
        };
        if ty == NtfsAttributeType::ReparsePoint {
            *parts.is_reparse = true;
            continue;
        }
        let part_file = entry
            .to_file(ntfs, fs)
            .map_err(|e| bad(tr!("ntfs_scan.extension_record", error = e)))?;
        // Чужая запись под видом нашей extension-записи — цепочка битая, и
        // экстентам из неё верить нельзя. Своя — это живая запись того
        // поколения, на которое ссылается список, и ссылается она на это
        // поколение базовой (номер И sequence), а не на прежнего владельца
        // того же номера.
        let wanted = base | (file.sequence_number() as u64) << 48;
        let entry_ref = entry.base_file_reference();
        if base_reference_of(fs, &part_file) != Some(wanted)
            || !part_file.flags().contains(ntfs::NtfsFileFlags::IN_USE)
            || entry_ref.sequence_number() != part_file.sequence_number()
        {
            return Err(bad(tr!(
                "ntfs_scan.extension_foreign",
                record = part_file.file_record_number(),
                base = base
            )));
        }
        let part_attr = entry
            .to_attribute(&part_file)
            .map_err(|e| bad(tr!("ntfs_scan.extension_attribute", error = e)))?;
        if ty == NtfsAttributeType::FileName {
            let fname = part_attr
                .structured_value::<_, NtfsFileName>(fs)
                .map_err(|e| bad(tr!("ntfs_scan.extension_attribute", error = e)))?;
            push_name(parts.names, &fname);
            continue;
        }
        let name = entry.name().0.to_vec();
        let s = stream_mut(parts.data, parts.named, &name);
        if !take_data_part(&part_attr, fs, s, part_file.file_record_number()) {
            return Err(bad(tr!("ntfs_scan.extension_runs_unparsed")));
        }
    }
    Ok(())
}

fn bad(msg: impl std::fmt::Display) -> OwnBackupError {
    OwnBackupError::Device(format!("MFT scan: {msg}"))
}

/// `Read + Seek` поверх `RawVolume` — расшифрованного окна BitLocker, сырого
/// устройства, чего угодно, что умеет читать по смещению. Так проход по MFT
/// работает над источником, который Windows не смонтировала.
pub struct RawVolumeCursor<'a> {
    vol: &'a mut dyn crate::fat_map::RawVolume,
    pos: u64,
}

impl<'a> RawVolumeCursor<'a> {
    pub fn new(vol: &'a mut dyn crate::fat_map::RawVolume) -> Self {
        Self { vol, pos: 0 }
    }
}

impl Read for RawVolumeCursor<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let len = self.vol.len();
        if self.pos >= len || buf.is_empty() {
            return Ok(0);
        }
        let take = buf.len().min((len - self.pos) as usize);
        self.vol
            .read_at(self.pos, &mut buf[..take])
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        self.pos += take as u64;
        Ok(take)
    }
}

impl Seek for RawVolumeCursor<'_> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.pos = match pos {
            SeekFrom::Start(v) => v,
            SeekFrom::Current(v) => (self.pos as i64 + v) as u64,
            SeekFrom::End(v) => (self.vol.len() as i64 + v) as u64,
        };
        Ok(self.pos)
    }
}

/// Кэш крупными кусками поверх тома.
///
/// Крейт `ntfs` читает каждую запись MFT отдельным `read_exact` на 1 КиБ, и на
/// миллионе записей это миллион обращений к носителю: первый прогон по
/// настоящему образу занял 313 с против 68 с у Python-пробы, которая читала MFT
/// кусками по 4 МиБ. Разница — только в размере чтения, поэтому здесь
/// маленький кольцевой кэш: MFT читается последовательно и попадает в него
/// почти всегда, а прыжки за extension-записями (ATTRIBUTE_LIST) держит
/// несколько окон сразу.
pub struct BufferedVolume<R> {
    inner: R,
    /// Окна кэша: (начало, данные). Замена — по кругу.
    slots: Vec<(u64, Vec<u8>)>,
    next_slot: usize,
    window: u64,
    pos: u64,
}

impl<R: Read + Seek> BufferedVolume<R> {
    pub fn new(inner: R, window: u64, slots: usize) -> Self {
        Self {
            inner,
            slots: Vec::with_capacity(slots.max(1)),
            next_slot: 0,
            window: window.max(4096),
            pos: 0,
        }
    }

    fn fill(&mut self, start: u64) -> std::io::Result<usize> {
        let mut buf = vec![0u8; self.window as usize];
        self.inner.seek(SeekFrom::Start(start))?;
        let mut got = 0usize;
        // Короткое чтение — это конец тома, а не ошибка.
        while got < buf.len() {
            match self.inner.read(&mut buf[got..]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        buf.truncate(got);
        let idx = if self.slots.len() < self.slots.capacity() {
            self.slots.push((start, buf));
            self.slots.len() - 1
        } else {
            let i = self.next_slot % self.slots.len();
            self.next_slot = self.next_slot.wrapping_add(1);
            self.slots[i] = (start, buf);
            i
        };
        Ok(idx)
    }
}

impl<R: Read + Seek> Read for BufferedVolume<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let start = self.pos / self.window * self.window;
        let idx = match self.slots.iter().position(|(s, _)| *s == start) {
            Some(i) => i,
            None => self.fill(start)?,
        };
        let (_, data) = &self.slots[idx];
        let off = (self.pos - start) as usize;
        if off >= data.len() {
            return Ok(0);
        }
        let n = buf.len().min(data.len() - off);
        buf[..n].copy_from_slice(&data[off..off + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl<R: Read + Seek> Seek for BufferedVolume<R> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.pos = match pos {
            SeekFrom::Start(v) => v,
            SeekFrom::Current(v) => (self.pos as i64 + v) as u64,
            SeekFrom::End(_) => {
                return Err(std::io::Error::other(
                    tr!("ntfs_scan.seek_from_end_unsupported").into_owned(),
                ))
            }
        };
        Ok(self.pos)
    }
}

/// Окно на том внутри чего-то большего (образа с таблицей разделов, диска):
/// `Read + Seek` со смещениями от начала ТОМА, как их ждёт крейт `ntfs`.
pub struct VolumeWindow<R> {
    inner: R,
    base: u64,
    pos: u64,
    len: u64,
}

impl<R: Read + Seek> VolumeWindow<R> {
    pub fn new(inner: R, base: u64, len: u64) -> Self {
        Self {
            inner,
            base,
            pos: 0,
            len,
        }
    }
}

impl<R: Read + Seek> Read for VolumeWindow<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.len {
            return Ok(0);
        }
        let take = buf.len().min((self.len - self.pos) as usize);
        self.inner.seek(SeekFrom::Start(self.base + self.pos))?;
        let n = self.inner.read(&mut buf[..take])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl<R: Read + Seek> Seek for VolumeWindow<R> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let new = match pos {
            SeekFrom::Start(v) => v as i64,
            SeekFrom::End(v) => self.len as i64 + v,
            SeekFrom::Current(v) => self.pos as i64 + v,
        };
        if new < 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                tr!("ntfs_scan.seek_before_start").into_owned(),
            ));
        }
        self.pos = new as u64;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// `Read + Seek` поверх образа VHDX — для приёмки на настоящей копии, снятой
    /// с живой машины. Работает и под Linux: `VhdxGuestReader` кроссплатформенный.
    struct VhdxCursor {
        inner: crate::vhdx_bat::VhdxGuestReader,
        pos: u64,
    }

    impl Read for VhdxCursor {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            self.inner
                .read_at(self.pos, buf)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            self.pos += buf.len() as u64;
            Ok(buf.len())
        }
    }

    impl Seek for VhdxCursor {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.pos = match pos {
                SeekFrom::Start(v) => v,
                SeekFrom::Current(v) => (self.pos as i64 + v) as u64,
                SeekFrom::End(_) => {
                    return Err(std::io::Error::other("seek от конца не поддержан"))
                }
            };
            Ok(self.pos)
        }
    }

    /// Найти начало тома: образ одного тома у нас обёрнут в GPT (том с 1 МиБ).
    fn volume_offset(cur: &mut VhdxCursor) -> u64 {
        let mut boot = [0u8; 512];
        for candidate in [0u64, 1 << 20] {
            cur.seek(SeekFrom::Start(candidate)).unwrap();
            if cur.read(&mut boot).is_ok() && &boot[3..11] == b"NTFS    " {
                return candidate;
            }
        }
        panic!("NTFS не найдена ни на 0, ни на 1 МиБ");
    }

    /// Приёмка B2: тот же образ, что мерил этап B0, и тот же вопрос — сходятся
    /// ли наши экстенты с ответом самой Windows (`FSCTL_GET_RETRIEVAL_POINTERS`,
    /// выписанный командой `scan-exclusions --list`).
    ///
    /// ```text
    /// OWN_BACKUP_NTFS_IMAGE=win-files/disk-f/…vhdx \
    /// OWN_BACKUP_WALK_BYTES=tmp/walk.txt.bytes \
    ///   cargo test -p own-backup-backup ntfs_scan -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "нужен настоящий образ NTFS: OWN_BACKUP_NTFS_IMAGE"]
    fn matches_windows_extents_on_a_real_image() {
        let Ok(image) = std::env::var("OWN_BACKUP_NTFS_IMAGE") else {
            eprintln!("нет OWN_BACKUP_NTFS_IMAGE — пропускаю");
            return;
        };
        let started = std::time::Instant::now();
        let reader = crate::vhdx_bat::VhdxGuestReader::open(std::path::Path::new(&image))
            .expect("открыть образ");
        let mut cur = VhdxCursor {
            inner: reader,
            pos: 0,
        };
        let base = volume_offset(&mut cur);
        let win = VolumeWindow::new(cur, base, u64::MAX / 2);
        // 4 МиБ × 6 окон: MFT читается последовательно, а прыжки за
        // extension-записями не выбивают текущее окно.
        let mut win = BufferedVolume::new(win, 4 << 20, 6);

        // Двенадцать правил, которые отдавал `core::default_exclusions()` во
        // время замеров (2026-08-20). С тех пор список по умолчанию другой
        // (2026-09-26), но этот набор оставлен как есть: замер сравним только
        // сам с собой. Брать его оттуда нельзя и так — `core` зависит от
        // `backup`, и обратная зависимость замкнула бы граф.
        let patterns: Vec<String> = [
            "pagefile.sys",
            "hiberfil.sys",
            "swapfile.sys",
            "$Recycle.Bin",
            "System Volume Information",
            "**\\Temp\\**",
            "**\\Tmp\\**",
            "*.tmp",
            "*.~",
            "*.vhdx",
            "*.avhdx",
            "*.obkm",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let scan = scan_mft(&mut win, &patterns).expect("скан MFT");
        let elapsed = started.elapsed();

        println!("записей MFT:      {}", scan.records_scanned);
        println!("совпало путей:    {}", scan.paths.len());
        println!("вычтено файлов:   {}", scan.files_excluded);
        println!(
            "вычтено байт:     {} ({:.0} МиБ)",
            scan.bytes_excluded,
            scan.bytes_excluded as f64 / 1048576.0
        );
        println!("оставлено в копии: {} (причины ниже)", scan.kept.len());
        let mut by_reason: std::collections::BTreeMap<&str, usize> = Default::default();
        for (_, r) in &scan.kept {
            *by_reason.entry(r.as_str()).or_default() += 1;
        }
        for (r, n) in &by_reason {
            println!("   {n:6}  {r}");
        }
        println!(
            "время:            {:.1} с (разбор {:.1} с + правила {:.1} с)",
            elapsed.as_secs_f64(),
            scan.parse_ms as f64 / 1000.0,
            scan.decide_ms as f64 / 1000.0
        );

        if let Ok(out) = std::env::var("OWN_BACKUP_SCAN_OUT") {
            let mut f = std::fs::File::create(&out).unwrap();
            let mut sorted = scan.file_bytes.clone();
            sorted.sort();
            for (p, b) in sorted {
                writeln!(f, "{b}\t{p}").unwrap();
            }
            println!("список путей:     {out}");
        }
        assert!(
            scan.files_excluded > 0,
            "не найдено ни одного исключаемого файла"
        );
    }
}

#[cfg(test)]
mod rules_tests {
    //! Правила исключения — отдельно от чтения NTFS.
    //!
    //! Записи здесь собраны руками, и это НЕ попытка изобразить настоящий том:
    //! разбор NTFS проверен сверкой с самой Windows (17 889 файлов байт-в-байт),
    //! а тут испытывается то, чего никакая сверка не покажет — решения. Часть
    //! из них на живом томе не встретилась ни разу: жёстких ссылок среди
    //! исключённых файлов там было ровно ноль.

    use super::*;
    use crate::exclusions::parse_patterns;

    fn dir(parent: u64, name: &str) -> Entry {
        Entry {
            names: vec![Name {
                parent,
                name: name.into(),
            }],
            is_dir: true,
            is_reparse: false,
            data: Stream::default(),
            named: Vec::new(),
            broken: false,
            names_complete: true,
        }
    }

    /// Поток с одним экстентом ровно по размеру.
    fn stream(lcn: u64, clusters: u64) -> Stream {
        Stream {
            size: clusters * 4096,
            extents: vec![(lcn * 4096, clusters * 4096)],
            allocated: clusters * 4096,
            compressed_or_sparse: false,
            resident: false,
        }
    }

    /// Поток внутри записи MFT.
    fn resident(size: u64) -> Stream {
        Stream {
            size,
            resident: true,
            ..Stream::default()
        }
    }

    /// Файл с одним именем и одним экстентом ровно по размеру.
    fn file(parent: u64, name: &str, lcn: u64, clusters: u64) -> Entry {
        Entry {
            names: vec![Name {
                parent,
                name: name.into(),
            }],
            is_dir: false,
            is_reparse: false,
            data: stream(lcn, clusters),
            named: Vec::new(),
            broken: false,
            names_complete: true,
        }
    }

    fn run(entries: &[(u64, Entry)], patterns: &[&str]) -> MftScan {
        let map: HashMap<u64, Entry> = entries.iter().cloned().collect();
        let parsed = parse_patterns(&patterns.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let mut scan = MftScan::default();
        decide(&map, &parsed, 4096, &mut scan);
        scan.ranges.finalize();
        scan
    }

    fn kept_reasons(scan: &MftScan) -> Vec<KeptReason> {
        scan.kept.iter().map(|(_, r)| *r).collect()
    }

    /// Жёсткая ссылка ПОПЕРЁК исключения: одно имя под правилом, второе — нет.
    /// Данные общие, и вычесть их значит выпотрошить уцелевшее имя. Урок xfs P3,
    /// на живом томе не встретился ни разу.
    #[test]
    fn a_hard_link_outside_the_exclusion_keeps_the_data() {
        let mut shared = file(20, "shared.tmp", 100, 2);
        shared.names.push(Name {
            parent: ROOT_RECORD,
            name: "keep-me.bin".into(),
        });
        let scan = run(&[(20, dir(ROOT_RECORD, "Temp")), (30, shared)], &["*.tmp"]);
        assert_eq!(
            scan.files_excluded, 0,
            "данные общие с уцелевшим именем — вычитать нельзя"
        );
        assert_eq!(kept_reasons(&scan), vec![KeptReason::OtherNameKept]);
        assert!(
            scan.ranges.is_empty(),
            "ни один кластер не должен быть вычтен"
        );
        // Запись из каталога всё равно убирается — под правило имя попало.
        assert_eq!(scan.paths.len(), 1);
    }

    /// Оба имени под исключением — данных не нужно никому.
    #[test]
    fn a_hard_link_excluded_under_every_name_is_subtracted() {
        let mut shared = file(20, "a.tmp", 100, 2);
        shared.names.push(Name {
            parent: 20,
            name: "b.tmp".into(),
        });
        let scan = run(&[(20, dir(ROOT_RECORD, "Temp")), (30, shared)], &["*.tmp"]);
        assert_eq!(scan.files_excluded, 1);
        assert_eq!(scan.bytes_excluded, 2 * 4096);
        assert!(scan.kept.is_empty());
    }

    /// Резидентный файл: данные внутри записи MFT, кластеров нет. Вычитать
    /// нечего, но из каталога он уходит.
    #[test]
    fn a_resident_file_keeps_its_data_and_loses_its_entry() {
        let mut small = file(ROOT_RECORD, "tiny.tmp", 0, 0);
        small.data = resident(300);
        let scan = run(&[(30, small)], &["*.tmp"]);
        assert_eq!(scan.files_excluded, 0);
        assert_eq!(kept_reasons(&scan), vec![KeptReason::Resident]);
        assert_eq!(scan.paths.len(), 1, "запись из каталога удаляется");
    }

    /// Экстенты не покрывают размер файла — значит run list прочитан не весь
    /// (ATTRIBUTE_LIST). Вычесть половину файла хуже, чем не вычитать: файл
    /// остаётся целиком. На реальном томе за этим пряталось 1079 МиБ.
    #[test]
    fn a_half_read_run_list_keeps_the_whole_file() {
        let mut torn = file(ROOT_RECORD, "big.tmp", 100, 2);
        torn.data.size = 100 * 4096; // заявлено 100 кластеров, найдено 2
        let scan = run(&[(30, torn)], &["*.tmp"]);
        assert_eq!(scan.files_excluded, 0);
        assert_eq!(kept_reasons(&scan), vec![KeptReason::ExtentsIncomplete]);
        assert!(scan.ranges.is_empty());
    }

    /// Файл без единого экстента, но не пустой — тот же случай, только грубее.
    #[test]
    fn a_file_with_no_extents_at_all_is_kept() {
        let mut hidden = file(ROOT_RECORD, "x.tmp", 0, 0);
        hidden.data.size = 954 * 1024 * 1024;
        hidden.data.extents.clear();
        hidden.data.allocated = 0;
        let scan = run(&[(30, hidden)], &["*.tmp"]);
        assert_eq!(kept_reasons(&scan), vec![KeptReason::ExtentsIncomplete]);
    }

    /// Точка повторного разбора: удаляем саму точку, к цели не прикасаемся.
    #[test]
    fn a_reparse_point_keeps_its_target() {
        let mut link = file(ROOT_RECORD, "junction.tmp", 100, 2);
        link.is_reparse = true;
        let scan = run(&[(30, link)], &["*.tmp"]);
        assert_eq!(kept_reasons(&scan), vec![KeptReason::Reparse]);
        assert!(scan.ranges.is_empty(), "цель junction'а не наша");
    }

    /// Содержимое исключённого каталога вычитается, но ОТДЕЛЬНОЙ записи на
    /// удаление не получает: каталог унесёт его с собой.
    #[test]
    fn files_inside_an_excluded_directory_ride_along() {
        let scan = run(
            &[
                (20, dir(ROOT_RECORD, "Temp")),
                (21, dir(20, "nested")),
                (30, file(20, "a.bin", 100, 1)),
                (31, file(21, "b.bin", 200, 1)),
            ],
            &["Temp"],
        );
        assert_eq!(scan.files_excluded, 2, "оба файла вычтены");
        assert_eq!(scan.bytes_excluded, 2 * 4096);
        assert_eq!(scan.paths.len(), 1, "на удаление — только сам каталог");
        assert_eq!(scan.paths[0].rel, "Temp");
        assert!(scan.paths[0].is_dir);
    }

    /// Служебные записи тома неприкосновенны, как бы ни совпало имя.
    #[test]
    fn the_volume_own_files_are_never_excluded() {
        let mut mft = file(ROOT_RECORD, "$MFT.tmp", 100, 8);
        mft.names[0].name = "$Boot.tmp".into();
        let scan = run(&[(7, mft)], &["*.tmp"]);
        assert_eq!(scan.files_excluded, 0, "запись 7 — служебная");
        assert!(scan.paths.is_empty());
        assert!(scan.kept.is_empty());
    }

    /// The `$Extend` tree is the volume's own, whatever its record numbers
    /// and whatever a rule says: `$UsnJrnl` with its `$J` stream, and
    /// `$RmMetadata\$Repair` deeper down. Cutting `$J` out of the copy would
    /// leave the image a journal of zeros.
    #[test]
    fn the_extend_tree_is_never_excluded() {
        let mut usn = file(EXTEND_RECORD, "$UsnJrnl", 0, 0);
        usn.data = Stream::default();
        usn.named.push(("$J".into(), stream(100, 8)));
        usn.named.push(("$Max".into(), resident(32)));
        let mut repair = file(40, "$Repair", 200, 2);
        repair.named.push(("$Config".into(), resident(8)));
        let scan = run(
            &[
                (EXTEND_RECORD, dir(ROOT_RECORD, "$Extend")),
                (40, dir(EXTEND_RECORD, "$RmMetadata")),
                (41, dir(40, "$TxfLog")),
                (42, file(41, "$TxfLog.blf", 300, 1)),
                (38, usn),
                (39, repair),
                (50, file(ROOT_RECORD, "user.tmp", 400, 1)),
            ],
            &[
                "$Extend",
                "$UsnJrnl",
                "$RmMetadata",
                "**\\$TxfLog\\**",
                "*.tmp",
                "$*",
            ],
        );
        let rels: Vec<&str> = scan.paths.iter().map(|p| p.rel.as_str()).collect();
        assert_eq!(rels, vec!["user.tmp"], "only the user's file");
        assert_eq!(scan.files_excluded, 1);
        assert!(covered(&scan, 400, 1));
        for (lcn, n) in [(100, 8), (200, 2), (300, 1)] {
            assert!(!covered(&scan, lcn, n), "metadata at {lcn} subtracted");
        }
        assert_eq!(scan.bytes_excluded, 4096);
    }

    /// `is_system` on a corrupt tree: a loop of parents ends, an orphan is no
    /// system file, and a child of `$Extend` deep down still is.
    #[test]
    fn is_system_survives_loops_and_orphans() {
        let map: HashMap<u64, Entry> = [
            (EXTEND_RECORD, dir(ROOT_RECORD, "$Extend")),
            (40, dir(EXTEND_RECORD, "$RmMetadata")),
            (41, dir(40, "$Txf")),
            (42, file(41, "deep", 1, 1)),
            (60, dir(61, "a")),
            (61, dir(60, "b")),
            (62, file(60, "in-loop.tmp", 2, 1)),
            (63, file(999, "orphan.tmp", 3, 1)),
            (64, file(ROOT_RECORD, "plain.tmp", 4, 1)),
        ]
        .into_iter()
        .collect();
        assert!(is_system(3, &map), "a record below 16");
        assert!(is_system(42, &map), "deep in $Extend");
        assert!(!is_system(62, &map), "a loop of parents ends");
        assert!(!is_system(63, &map), "an orphan");
        assert!(!is_system(64, &map));
        // The rules over the same tree: the loop and the orphan are left alone.
        let scan = run(&map.into_iter().collect::<Vec<_>>(), &["*.tmp"]);
        let rels: Vec<&str> = scan.paths.iter().map(|p| p.rel.as_str()).collect();
        assert_eq!(rels, vec!["plain.tmp"]);
    }

    /// A hard link is judged by every directory above each of its names, not
    /// only its parent: `Temp\deep\a.bin` is under the rule `Temp` though
    /// `deep` itself matches nothing. And a name whose parents loop is no
    /// excluded name — and no stack overflow.
    #[test]
    fn every_name_is_judged_by_its_whole_path() {
        let mut linked = file(21, "a.bin", 100, 1);
        linked.names.push(Name {
            parent: 20,
            name: "b.bin".into(),
        });
        let mut looped = file(20, "c.bin", 200, 1);
        looped.names.push(Name {
            parent: 60,
            name: "d.bin".into(),
        });
        let scan = run(
            &[
                (20, dir(ROOT_RECORD, "Temp")),
                (21, dir(20, "deep")),
                (30, linked),
                (31, looped),
                (60, dir(61, "x")),
                (61, dir(60, "y")),
            ],
            &["Temp"],
        );
        assert!(covered(&scan, 100, 1), "both names are under Temp");
        assert!(!covered(&scan, 200, 1), "a name outside any known path");
        assert_eq!(scan.files_excluded, 1);
        assert!(scan
            .kept
            .iter()
            .any(|(p, r)| p.ends_with("c.bin") && *r == KeptReason::OtherNameKept));
    }

    /// Кириллица и пробелы в пути: матчер работает по компонентам, регистр не
    /// важен. На живом томе такие пути были у каждого второго файла.
    #[test]
    fn cyrillic_and_spaces_match_by_component() {
        let scan = run(
            &[
                (20, dir(ROOT_RECORD, "Пользователи")),
                (21, dir(20, "Володя Большой")),
                (22, dir(21, "TEMP")),
                (30, file(22, "кэш.bin", 100, 3)),
            ],
            &["**\\Temp\\**"],
        );
        assert_eq!(scan.files_excluded, 1, "TEMP и Temp — одно и то же");
        assert_eq!(scan.bytes_excluded, 3 * 4096);
    }

    /// Разреженный файл: дыры места не занимают, вычитается только реальное.
    #[test]
    fn a_sparse_file_subtracts_only_what_it_occupies() {
        let mut sparse = file(ROOT_RECORD, "sparse.tmp", 100, 2);
        sparse.data.size = 64 * 4096; // 64 кластера «по размеру», выделено 2
        sparse.data.allocated = 2 * 4096;
        // Признак разреженности берётся у АТРИБУТА. Без него файл, занимающий
        // меньше своего размера, неотличим от недочитанного run list'а — и
        // должен считаться недочитанным, потому что ошибиться в эту сторону
        // безопаснее.
        sparse.data.compressed_or_sparse = true;
        let scan = run(&[(30, sparse)], &["*.tmp"]);
        assert_eq!(
            scan.files_excluded, 1,
            "выделенное кратно кластеру — файл сжат/разрежен"
        );
        assert_eq!(scan.bytes_excluded, 2 * 4096);
    }

    /// Оборванная цепочка родителей: путь не собрать, правила не применить.
    /// Такую запись нельзя ни вычесть, ни удалить — только сосчитать.
    #[test]
    fn an_orphan_record_is_left_alone() {
        let scan = run(&[(30, file(999, "orphan.tmp", 100, 1))], &["*.tmp"]);
        assert_eq!(scan.files_excluded, 0);
        assert!(scan.paths.is_empty());
        assert!(scan.ranges.is_empty());
    }

    /// The records whose content the deletion clears: a file is whole when
    /// every name it has is excluded — a short 8.3 alias is not a name (the
    /// scan never records it), a hard link that stays is — and listed when it
    /// holds something inside its record. Files inside an excluded directory
    /// are listed under that directory's entry; a record whose names may not
    /// all have been read is never listed.
    #[test]
    fn whole_records_are_those_every_name_of_which_goes() {
        // A short resident stream, as most downloaded files carry one.
        let zoned = |mut e: Entry| {
            e.named.push(("Zone.Identifier".into(), resident(26)));
            e
        };
        let mut twin = zoned(file(21, "twin.bin", 300, 1));
        twin.names.push(Name {
            parent: ROOT_RECORD,
            name: "kept.bin".into(),
        });
        let mut both = zoned(file(20, "a.bin", 400, 1));
        both.names.push(Name {
            parent: ROOT_RECORD,
            name: "b.tmp".into(),
        });
        let mut listed = zoned(file(21, "listed.bin", 500, 1));
        listed.names_complete = false;
        let mut small = file(ROOT_RECORD, "Long resident name.tmp", 0, 0);
        small.data = resident(30);
        let mut shared = zoned(file(ROOT_RECORD, "shared.tmp", 600, 1));
        shared.names.push(Name {
            parent: ROOT_RECORD,
            name: "keep-me.bin".into(),
        });
        let scan = run(
            &[
                (20, dir(ROOT_RECORD, "Temp")),
                (21, dir(20, "nested")),
                (30, zoned(file(21, "x.bin", 100, 1))),
                (31, twin),
                (32, both),
                (33, listed),
                (34, file(21, "bare.bin", 700, 1)),
                (40, small),
                (41, shared),
            ],
            &["Temp", "*.tmp"],
        );
        let whole = |rel: &str| {
            scan.paths
                .iter()
                .find(|p| p.rel == rel)
                .unwrap_or_else(|| panic!("{rel} recorded"))
                .whole_records
                .clone()
        };
        assert_eq!(
            whole("Temp"),
            vec![30, 32],
            "under the directory; nothing in the record of bare.bin"
        );
        assert_eq!(whole("Long resident name.tmp"), vec![40]);
        assert!(whole("shared.tmp").is_empty(), "its other name stays");
        assert_eq!(scan.paths.len(), 3);
    }

    fn covered(scan: &MftScan, lcn: u64, clusters: u64) -> bool {
        let (start, end) = (lcn * 4096, (lcn + clusters) * 4096);
        scan.ranges.iter().any(|(s, e)| s <= start && end <= e)
    }

    /// A named stream (ADS) in clusters of its own is the file's content as
    /// much as its unnamed data: both leave the copy, and the bytes count both.
    /// Until 2026-10-23 the stream's clusters were copied.
    #[test]
    fn a_named_stream_leaves_the_copy_with_its_file() {
        let mut f = file(ROOT_RECORD, "x.tmp", 100, 2);
        f.named.push(("Zone.Identifier".into(), resident(26)));
        f.named.push(("hidden".into(), stream(300, 3)));
        let scan = run(&[(30, f)], &["*.tmp"]);
        assert_eq!(scan.files_excluded, 1);
        assert!(covered(&scan, 100, 2), "the unnamed data");
        assert!(covered(&scan, 300, 3), "the named stream");
        assert_eq!(scan.bytes_excluded, 5 * 4096);
        assert_eq!(scan.file_bytes, vec![("x.tmp".to_string(), 5 * 4096)]);
        assert!(scan.kept.is_empty());
        assert_eq!(
            scan.paths[0].whole_records,
            vec![30],
            "its resident stream is cleared"
        );
    }

    /// A small file keeps its content inside its record, yet its stream can
    /// be big: the stream goes, the resident part is the deletion's to clear.
    #[test]
    fn a_resident_file_still_loses_its_stream_clusters() {
        let mut f = file(ROOT_RECORD, "small.tmp", 0, 0);
        f.data = resident(40);
        f.named.push(("bulk".into(), stream(500, 4)));
        let scan = run(&[(30, f)], &["*.tmp"]);
        assert_eq!(kept_reasons(&scan), vec![KeptReason::Resident]);
        assert!(covered(&scan, 500, 4));
        assert_eq!(scan.bytes_excluded, 4 * 4096);
        assert_eq!(scan.files_excluded, 0, "its content is still in the copy");
        assert_eq!(scan.paths[0].whole_records, vec![30]);
    }

    /// A stream whose run list was not read in full keeps the whole file, as
    /// a torn unnamed run list does: half a stream subtracted is worse than
    /// none.
    #[test]
    fn a_torn_named_stream_keeps_the_whole_file() {
        let mut f = file(ROOT_RECORD, "torn.tmp", 100, 2);
        let mut torn = stream(300, 2);
        torn.size = 50 * 4096;
        f.named.push(("hidden".into(), torn));
        let scan = run(&[(30, f)], &["*.tmp"]);
        assert_eq!(kept_reasons(&scan), vec![KeptReason::ExtentsIncomplete]);
        assert!(scan.ranges.is_empty());
        assert_eq!(scan.bytes_excluded, 0);
        // A sparse stream legitimately occupies less than its size.
        let mut f = file(ROOT_RECORD, "sparse.tmp", 100, 2);
        let mut sparse = stream(300, 2);
        sparse.size = 50 * 4096;
        sparse.compressed_or_sparse = true;
        f.named.push(("hidden".into(), sparse));
        let scan = run(&[(30, f)], &["*.tmp"]);
        assert!(scan.kept.is_empty());
        assert!(covered(&scan, 300, 2));
    }

    /// Streams belong to the record, not to a name: with a hard link outside
    /// the exclusion, or behind a reparse point, or in a record that did not
    /// parse, none of them goes.
    #[test]
    fn streams_stay_where_the_file_stays() {
        let mut linked = file(20, "shared.tmp", 100, 1);
        linked.names.push(Name {
            parent: ROOT_RECORD,
            name: "keep-me.bin".into(),
        });
        linked.named.push(("s".into(), stream(200, 1)));
        let mut small_linked = file(20, "small.tmp", 0, 0);
        small_linked.data = resident(10);
        small_linked.names.push(Name {
            parent: ROOT_RECORD,
            name: "small-keep.bin".into(),
        });
        small_linked.named.push(("s".into(), stream(250, 1)));
        let mut link = file(20, "junction.tmp", 300, 1);
        link.is_reparse = true;
        link.named.push(("s".into(), stream(400, 1)));
        let mut broken = file(20, "broken.tmp", 500, 1);
        broken.broken = true;
        broken.named.push(("s".into(), stream(600, 1)));
        let scan = run(
            &[
                (20, dir(ROOT_RECORD, "Temp")),
                (30, linked),
                (31, small_linked),
                (32, link),
                (33, broken),
            ],
            &["*.tmp"],
        );
        assert!(
            scan.ranges.is_empty(),
            "{:?}",
            scan.ranges.iter().collect::<Vec<_>>()
        );
        assert_eq!(scan.bytes_excluded, 0);
    }

    /// An excluded directory's named streams go with it; those of the folder
    /// Windows recreates (it stays, emptied) stay.
    #[test]
    fn an_excluded_directory_takes_its_streams() {
        let mut cache = dir(ROOT_RECORD, "cache");
        cache
            .named
            .push(("com.dropbox.attrs".into(), stream(100, 1)));
        let mut inner = dir(20, "inner");
        inner.named.push(("s".into(), stream(150, 1)));
        let mut svi = dir(ROOT_RECORD, "System Volume Information");
        svi.named.push(("s".into(), stream(200, 1)));
        let mut kept = dir(ROOT_RECORD, "keep");
        kept.named.push(("s".into(), stream(300, 1)));
        let scan = run(
            &[(20, cache), (21, inner), (22, svi), (23, kept)],
            &["cache", "System Volume Information"],
        );
        assert!(covered(&scan, 100, 1));
        assert!(covered(&scan, 150, 1), "inside the excluded tree");
        assert!(
            !covered(&scan, 200, 1),
            "the folder Windows recreates stays"
        );
        assert!(!covered(&scan, 300, 1), "not excluded");
        assert_eq!(scan.bytes_excluded, 2 * 4096);
    }
}

/// The parser against a volume another writer made: `mkntfs` and `ntfscp`
/// from ntfs-3g, which run as an ordinary user on a plain file. A named
/// stream is written by `ntfscp -N`; thirty of them overflow the base record,
/// so ntfs-3g moves streams and the file's very name into extension records
/// behind an `$ATTRIBUTE_LIST` — the case a reader that stays in the base
/// record gets wrong without a word. Every excluded byte is a fill byte of
/// its own, and the oracle is the volume itself: each cluster that holds an
/// excluded fill must be subtracted, no cluster holding a kept fill may be.
/// Skipped where ntfs-3g is not installed.
#[cfg(all(test, target_os = "linux"))]
mod volume_tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    const KEEP: u8 = 0x5A;
    const KEEP_STREAM: u8 = 0x5B;
    const EXCLUDED: [u8; 5] = [0xAB, 0xCD, 0xCE, 0xC1, 0xC2];

    fn tool(name: &str) -> Option<PathBuf> {
        ["/usr/sbin", "/sbin", "/usr/bin", "/bin"]
            .iter()
            .map(|d| Path::new(d).join(name))
            .find(|p| p.exists())
    }

    fn sh(cmd: &mut Command) -> String {
        let out = cmd.output().expect("run");
        assert!(
            out.status.success(),
            "{cmd:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    struct Vol {
        _dir: tempfile::TempDir,
        image: PathBuf,
        ntfscp: PathBuf,
        ntfsinfo: PathBuf,
    }

    impl Vol {
        fn new() -> Option<Self> {
            let (Some(mkntfs), Some(ntfscp), Some(ntfsinfo)) =
                (tool("mkntfs"), tool("ntfscp"), tool("ntfsinfo"))
            else {
                eprintln!("skipped: mkntfs, ntfscp and ntfsinfo (ntfs-3g) are needed");
                return None;
            };
            let dir = tempfile::tempdir().unwrap();
            let image = dir.path().join("v.ntfs");
            std::fs::File::create(&image)
                .unwrap()
                .set_len(32 << 20)
                .unwrap();
            sh(Command::new(mkntfs)
                .args(["-F", "-Q", "-q", "-c", "4096"])
                .arg(&image));
            Some(Self {
                _dir: dir,
                image,
                ntfscp,
                ntfsinfo,
            })
        }

        /// Write `data` into `file` at the root — its stream `stream`, or its
        /// unnamed data.
        fn put(&self, file: &str, stream: Option<&str>, data: &[u8]) {
            let src = self.image.with_extension("src");
            std::fs::write(&src, data).unwrap();
            let mut cmd = Command::new(&self.ntfscp);
            cmd.arg("-q");
            if let Some(s) = stream {
                cmd.args(["-N", s]);
            }
            sh(cmd.arg(&self.image).arg(&src).arg(file));
        }

        fn info(&self, file: &str) -> String {
            sh(Command::new(&self.ntfsinfo)
                .arg("-F")
                .arg(format!("/{file}"))
                .arg(&self.image))
        }

        fn scan(&self) -> MftScan {
            let mut f = std::fs::File::open(&self.image).unwrap();
            scan_mft(&mut f, &["*.tmp".to_string()]).unwrap()
        }

        /// The clusters whose first bytes are all one fill byte, by that byte.
        fn fills(&self) -> Vec<(u8, u64)> {
            let data = std::fs::read(&self.image).unwrap();
            data.chunks(4096)
                .enumerate()
                .filter_map(|(i, c)| {
                    let b = c[0];
                    (c[..512].iter().all(|&x| x == b)
                        && (EXCLUDED.contains(&b) || b == KEEP || b == KEEP_STREAM))
                        .then_some((b, i as u64 * 4096))
                })
                .collect()
        }
    }

    fn subtracted(scan: &MftScan, at: u64) -> bool {
        scan.ranges.iter().any(|(s, e)| s <= at && at + 4096 <= e)
    }

    /// Build the volume every test here reads.
    fn populated() -> Option<Vol> {
        let v = Vol::new()?;
        // Unnamed and named, non-resident and resident.
        v.put("excl.tmp", None, &[0xAB; 9000]);
        v.put("excl.tmp", Some("big"), &[0xCD; 20_000]);
        v.put(
            "excl.tmp",
            Some("Zone.Identifier"),
            b"[ZoneTransfer]\r\nZoneId=3\r\n",
        );
        // Resident content, a stream in clusters of its own.
        v.put("small.tmp", None, b"RESIDENT-SECRET");
        v.put("small.tmp", Some("bulk"), &[0xCE; 12_000]);
        // An attribute list: the streams overflow the base record.
        v.put("many.tmp", None, &[0xC1; 30_000]);
        for i in 0..30 {
            let mut s = b"LIST-SECRET-".to_vec();
            s.resize(100, b'q');
            v.put("many.tmp", Some(&format!("s{i}")), &s);
        }
        v.put("many.tmp", Some("bigx"), &[0xC2; 40_000]);
        // Kept, with a stream of its own.
        v.put("keep.bin", None, &[KEEP; 30_000]);
        v.put("keep.bin", Some("keepstream"), &[KEEP_STREAM; 20_000]);
        let many = v.info("many.tmp");
        assert!(
            many.contains("$ATTRIBUTE_LIST"),
            "ntfs-3g did not make an attribute list:\n{many}"
        );
        Some(v)
    }

    #[test]
    fn every_stream_of_an_excluded_file_is_subtracted() {
        let Some(v) = populated() else {
            return;
        };
        let scan = v.scan();
        let fills = v.fills();
        for b in EXCLUDED {
            assert!(
                fills.iter().any(|(f, _)| *f == b),
                "no cluster of fill {b:#x} — the volume is not what the test thinks"
            );
        }
        for (b, at) in &fills {
            if EXCLUDED.contains(b) {
                assert!(
                    subtracted(&scan, *at),
                    "fill {b:#x} at {at} stays in the copy"
                );
            } else {
                assert!(
                    !subtracted(&scan, *at),
                    "kept fill {b:#x} at {at} subtracted"
                );
            }
        }
        // Two files gone whole; the small one keeps its record's content.
        assert_eq!(scan.files_excluded, 2, "{:?}", scan.kept);
        assert_eq!(
            scan.kept,
            vec![("small.tmp".to_string(), KeptReason::Resident)]
        );
        // The extension records are no files of their own: three paths, and
        // each whole — `many.tmp`'s name lives in an extension record.
        let mut paths: Vec<_> = scan
            .paths
            .iter()
            .map(|p| (p.rel.clone(), p.whole_records.len()))
            .collect();
        paths.sort();
        assert_eq!(
            paths,
            vec![
                ("excl.tmp".to_string(), 1),
                ("many.tmp".to_string(), 1),
                ("small.tmp".to_string(), 1)
            ]
        );
    }

    /// An attribute list that points into a record which is not a live
    /// extension of this generation of this file is a broken chain, and
    /// nothing of the file is subtracted: a record extending another file, an
    /// extension of an older generation of this record number, one no longer
    /// in use, one reused since the list was written (its own sequence number
    /// is not the one the list names).
    #[test]
    fn a_foreign_extension_record_voids_the_file() {
        /// Header offsets of an MFT record: sequence number, flags.
        const SEQUENCE: u64 = 0x10;
        const FLAGS: u64 = 0x16;
        for forgery in ["foreign", "stale generation", "not in use", "reused"] {
            let Some(v) = populated() else {
                return;
            };
            // The record holding `bigx`, from ntfsinfo's dump of the file.
            let info = v.info("many.tmp");
            let lines: Vec<&str> = info.lines().collect();
            let i = lines
                .iter()
                .position(|l| l.contains("'bigx'"))
                .expect("bigx in the dump");
            let header = lines[..i]
                .iter()
                .rev()
                .find(|l| l.starts_with("Dumping attribute $DATA"))
                .unwrap();
            let record: u64 = header
                .rsplit("from mft record ")
                .next()
                .and_then(|t| t.split_whitespace().next())
                .and_then(|n| n.parse().ok())
                .expect("record number");
            let (at, reference, sequence) = {
                let mut f = std::fs::File::open(&v.image).unwrap();
                let ntfs = Ntfs::new(&mut f).unwrap();
                let file = ntfs.file(&mut f, record).unwrap();
                let reference = base_reference_of(&mut f, &file).unwrap();
                assert_ne!(reference & RECORD_MASK, 0, "an extension record");
                assert!(file.flags().contains(ntfs::NtfsFileFlags::IN_USE));
                (
                    file.position().value().unwrap().get(),
                    reference,
                    file.sequence_number(),
                )
            };
            let (offset, bytes): (u64, Vec<u8>) = match forgery {
                // Its base reference at the root directory…
                "foreign" => (BASE_REFERENCE_OFFSET, ROOT_RECORD.to_le_bytes().to_vec()),
                // …at the same base record one generation earlier…
                "stale generation" => (
                    BASE_REFERENCE_OFFSET,
                    reference.wrapping_sub(1 << 48).to_le_bytes().to_vec(),
                ),
                // …the record freed…
                "not in use" => (FLAGS, 0u16.to_le_bytes().to_vec()),
                // …or given out again under a new sequence number.
                _ => (SEQUENCE, sequence.wrapping_add(1).to_le_bytes().to_vec()),
            };
            {
                use std::os::unix::fs::FileExt;
                let f = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&v.image)
                    .unwrap();
                f.write_all_at(&bytes, at + offset).unwrap();
            }
            let scan = v.scan();
            assert!(
                scan.kept
                    .contains(&("many.tmp".to_string(), KeptReason::Unreadable)),
                "{forgery}: {:?}",
                scan.kept
            );
            for (b, at) in v.fills() {
                if b == 0xC1 || b == 0xC2 {
                    assert!(
                        !subtracted(&scan, at),
                        "{forgery}: {b:#x} of a broken record subtracted"
                    );
                }
            }
            let many = scan.paths.iter().find(|p| p.rel == "many.tmp").unwrap();
            assert!(
                many.whole_records.is_empty(),
                "{forgery}: its names may be incomplete"
            );
        }
    }
}
