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
}

/// Одно имя файла в MFT.
struct Name {
    parent: u64,
    name: String,
}

/// Живая запись MFT, разобранная первым проходом.
struct Entry {
    names: Vec<Name>,
    is_dir: bool,
    is_reparse: bool,
    /// Размер несжатого потока данных (0 у каталогов и пустых файлов).
    data_size: u64,
    /// (смещение на томе, длина) выделенных кластеров.
    extents: Vec<(u64, u64)>,
    /// Сумма длин экстентов — насколько ФС выделила места под файл.
    allocated: u64,
    /// Сжат или разрежен по флагам АТРИБУТА, а не по догадке. У таких файлов
    /// выделено законно меньше, чем они весят, и путать это с недочитанным
    /// run list'ом нельзя — см. `decide`.
    compressed_or_sparse: bool,
    resident_data: bool,
    /// Разбор атрибутов упёрся в ошибку — доверять экстентам нельзя.
    broken: bool,
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
        if info.flags().contains(ntfs::structured_values::NtfsVolumeFlags::IS_DIRTY) {
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

    // --- проход 1: разобрать записи -----------------------------------------
    let mut entries: HashMap<u64, Entry> = HashMap::with_capacity(total as usize / 2);
    for n in 0..total {
        scan.records_scanned += 1;
        // Раз в 4096 записей: на миллионе это 260 сообщений, а не миллион.
        if scan.records_scanned % 4096 == 0 {
            tick(scan.records_scanned, total);
        }
        let file = match ntfs.file(fs, n) {
            Ok(f) => f,
            // Мёртвая или неполная запись — таких в MFT штатно много.
            Err(_) => continue,
        };
        if !file.flags().contains(ntfs::NtfsFileFlags::IN_USE) {
            continue;
        }
        let is_dir = file.is_directory();

        let mut names = Vec::new();
        let mut extents: Vec<(u64, u64)> = Vec::new();
        let mut allocated = 0u64;
        let mut data_size = 0u64;
        let mut resident_data = false;
        let mut compressed_or_sparse = false;
        let mut broken = false;
        let mut is_reparse = false;

        // Атрибуты базовой записи. Плоский вид крейта (`attributes()`) сам
        // ходит по ATTRIBUTE_LIST, но отдаёт связанные данные ПОТОКОМ, без
        // списка экстентов — а нам нужны именно экстенты. Поэтому базовую
        // запись читаем сырым итератором, а за списком идём сами (ниже).
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
                Ok(NtfsAttributeType::FileName) => {
                    match attr.structured_value::<_, NtfsFileName>(fs) {
                        Ok(fname) => {
                            // Короткое имя 8.3 — псевдоним, а не отдельная ссылка.
                            if fname.namespace() != NtfsFileNamespace::Dos {
                                names.push(Name {
                                    parent: fname
                                        .parent_directory_reference()
                                        .file_record_number(),
                                    name: fname.name().to_string_lossy(),
                                });
                            }
                        }
                        Err(_) => broken = true,
                    }
                }
                Ok(NtfsAttributeType::ReparsePoint) => is_reparse = true,
                Ok(NtfsAttributeType::AttributeList) => attr_list_present = true,
                Ok(NtfsAttributeType::Data) => {
                    // Только безымянный поток: именованные (ADS) едут вместе с
                    // файлом и отдельно не вычитаются.
                    if attr.name_length() != 0 {
                        continue;
                    }
                    match attr.value(fs) {
                        Ok(NtfsAttributeValue::Resident(v)) => {
                            resident_data = true;
                            data_size = data_size.max(v.len());
                        }
                        Ok(NtfsAttributeValue::NonResident(v)) => {
                            use ntfs::NtfsAttributeFlags as F;
                            let f = attr.flags();
                            if f.contains(F::COMPRESSED) || f.contains(F::SPARSE) {
                                compressed_or_sparse = true;
                            }
                            // Длинный run list NTFS кладёт в НЕСКОЛЬКО
                            // экземпляров $DATA, и размер заполнен только у
                            // первого (StartVCN=0) — экстенты накапливаем,
                            // размер берём максимальный (проба B0 на этом
                            // спотыкалась и теряла крупные файлы).
                            data_size = data_size.max(v.len());
                            take_runs(v.data_runs(), &mut extents, &mut allocated, &mut broken, n);
                        }
                        Ok(NtfsAttributeValue::AttributeListNonResident(v)) => {
                            data_size = data_size.max(v.len());
                        }
                        Err(e) => {
                            warn!(record = n, error = %e, "MFT scan: значение $DATA не прочитано");
                            broken = true;
                        }
                    }
                }
                _ => {}
            }
        }

        // ATTRIBUTE_LIST: части $DATA лежат в других записях MFT. Без этого
        // прохода у крупного фрагментированного файла экстентов ноль — на
        // реальном томе так пряталось 954 МиБ в одном файле (замер B0).
        if attr_list_present {
            match read_attribute_list_data(&ntfs, fs, &file) {
                Ok((runs, alloc)) => {
                    extents.extend(runs);
                    allocated += alloc;
                }
                Err(e) => {
                    warn!(record = n, error = %e, "MFT scan: список атрибутов не пройден");
                    broken = true;
                }
            }
        }

        if names.is_empty() {
            continue; // extension-запись или запись без имени
        }
        entries.insert(
            n,
            Entry {
                names,
                is_dir,
                is_reparse,
                data_size,
                extents,
                allocated,
                compressed_or_sparse,
                resident_data,
                broken,
            },
        );
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
    debug!(parse_ms = scan.parse_ms, decide_ms = scan.decide_ms, "MFT scan: готово");
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
        let Some(last) = comps.last() else { return false };
        let lower: Vec<String> = comps.iter().map(|c| c.to_lowercase()).collect();
        let name_lower = last.to_lowercase();
        parsed.iter().any(|p| p.matches(&name_lower, &lower))
    };

    // Каталоги, попавшие под правила: всё под ними исключается вместе с ними.
    let mut excluded_dirs: std::collections::HashSet<u64> = std::collections::HashSet::new();
    for (&n, e) in entries {
        if !e.is_dir || n < FIRST_USER_RECORD {
            continue;
        }
        if let Some(p) = path_of(n, &entries, &mut path_cache, 0) {
            if !p.is_empty() && matches(&p) {
                excluded_dirs.insert(n);
            }
        }
    }

    let mut unresolved = 0u64;
    for (&n, e) in entries {
        if n < FIRST_USER_RECORD {
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
                scan.categories.add(&lower, e.data_size);
            }
            continue;
        }
        let rel = p.join("\\");
        // Верхняя точка совпадения: то, что попало под исключение только
        // потому, что лежит внутри исключённого каталога, отдельной записи не
        // получает — каталог унесёт его с собой.
        if !inherited {
            scan.paths.push(MatchedPath {
                rel: rel.clone(),
                is_dir: e.is_dir,
                is_reparse: e.is_reparse,
            });
        }
        if e.is_dir {
            continue;
        }
        let rel_for_bytes = rel.clone();

        // Данные остаются в копии, если...
        let reason = if e.broken {
            Some(KeptReason::Unreadable)
        } else if e.is_reparse {
            Some(KeptReason::Reparse)
        } else if e.resident_data {
            Some(KeptReason::Resident)
        } else if e.names.len() > 1 && !all_names_excluded(e, &entries, &mut path_cache, &matches, &excluded_dirs)
        {
            Some(KeptReason::OtherNameKept)
        } else if e.data_size > 0 && e.allocated == 0 {
            // Файл не пуст, а экстентов нет: run list не прочитан (за
            // ATTRIBUTE_LIST или битая запись). Молчать тут нельзя.
            Some(KeptReason::ExtentsIncomplete)
        } else if e.allocated < round_up(e.data_size, cluster_size) && !e.compressed_or_sparse {
            Some(KeptReason::ExtentsIncomplete)
        } else {
            None
        };

        match reason {
            Some(r) => {
                // Пофайлово — в debug: таких на живом томе под сотню, и в логе
                // они забивают всё остальное. Наружу идёт одна сводка ниже.
                debug!(file = %rel, reason = r.as_str(), "MFT scan: данные остаются в копии");
                scan.kept.push((rel, r));
            }
            None => {
                for (off, len) in &e.extents {
                    scan.ranges.add(*off, off + len);
                }
                scan.files_excluded += 1;
                scan.bytes_excluded += e.allocated;
                scan.file_bytes.push((rel_for_bytes, e.allocated));
            }
        }
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
    }

}

fn under_excluded(
    n: u64,
    entries: &HashMap<u64, Entry>,
    excluded: &std::collections::HashSet<u64>,
) -> bool {
    let mut cur = n;
    for _ in 0..128 {
        let Some(e) = entries.get(&cur) else { return false };
        let Some(name) = e.names.first() else { return false };
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
        let Some(mut parent_path) = path_of_pub(name.parent, entries, cache) else {
            return false;
        };
        parent_path.push(name.name.clone());
        if !(matches(&parent_path) || excluded.contains(&name.parent)) {
            return false;
        }
    }
    true
}

fn path_of_pub(
    n: u64,
    entries: &HashMap<u64, Entry>,
    cache: &mut HashMap<u64, Option<Vec<String>>>,
) -> Option<Vec<String>> {
    if n == ROOT_RECORD {
        return Some(Vec::new());
    }
    if let Some(hit) = cache.get(&n) {
        return hit.clone();
    }
    let entry = entries.get(&n)?;
    let name = entry.names.first()?;
    if name.parent == n {
        return None;
    }
    let up = path_of_pub(name.parent, entries, cache)?;
    let mut out = up;
    out.push(name.name.clone());
    cache.insert(n, Some(out.clone()));
    Some(out)
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

/// Пройти ATTRIBUTE_LIST и собрать экстенты всех частей безымянного $DATA,
/// живущих в других записях MFT.
fn read_attribute_list_data<T>(
    ntfs: &Ntfs,
    fs: &mut T,
    file: &ntfs::NtfsFile<'_>,
) -> Result<(Vec<(u64, u64)>, u64)>
where
    T: Read + Seek,
{
    let mut out = Vec::new();
    let mut allocated = 0u64;

    let mut list_attr = None;
    for attr in file.attributes_raw() {
        let attr = attr.map_err(|e| bad(tr!("ntfs_scan.attribute", error = e)))?;
        if attr.ty().ok() == Some(NtfsAttributeType::AttributeList) {
            list_attr = Some(attr);
            break;
        }
    }
    let Some(attr) = list_attr else {
        return Ok((out, 0));
    };
    let list = attr
        .structured_value::<_, NtfsAttributeList<'_, '_>>(fs)
        .map_err(|e| bad(tr!("ntfs_scan.attribute_list", error = e)))?;

    let mut entries = list.entries();
    while let Some(entry) = entries.next(fs) {
        let entry = entry.map_err(|e| bad(tr!("ntfs_scan.list_entry", error = e)))?;
        if entry.ty().ok() != Some(NtfsAttributeType::Data) || entry.name_length() != 0 {
            continue;
        }
        // Часть, лежащая в самой базовой записи, уже прочитана выше.
        if entry.base_file_reference().file_record_number() == file.file_record_number() {
            continue;
        }
        let part_file = entry
            .to_file(ntfs, fs)
            .map_err(|e| bad(tr!("ntfs_scan.extension_record", error = e)))?;
        let part_attr = entry
            .to_attribute(&part_file)
            .map_err(|e| bad(tr!("ntfs_scan.extension_attribute", error = e)))?;
        match part_attr.value(fs) {
            Ok(NtfsAttributeValue::NonResident(v)) => {
                let mut broken = false;
                take_runs(v.data_runs(), &mut out, &mut allocated, &mut broken, 0);
                if broken {
                    return Err(bad(tr!("ntfs_scan.extension_runs_unparsed")));
                }
            }
            Ok(_) => {}
            Err(e) => return Err(bad(tr!("ntfs_scan.extension_value", error = e))),
        }
    }
    Ok((out, allocated))
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
        Self { inner, base, pos: 0, len }
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
        let mut cur = VhdxCursor { inner: reader, pos: 0 };
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
        assert!(scan.files_excluded > 0, "не найдено ни одного исключаемого файла");
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
            names: vec![Name { parent, name: name.into() }],
            is_dir: true,
            is_reparse: false,
            data_size: 0,
            extents: Vec::new(),
            allocated: 0,
            compressed_or_sparse: false,
            resident_data: false,
            broken: false,
        }
    }

    /// Файл с одним именем и одним экстентом ровно по размеру.
    fn file(parent: u64, name: &str, lcn: u64, clusters: u64) -> Entry {
        Entry {
            names: vec![Name { parent, name: name.into() }],
            is_dir: false,
            is_reparse: false,
            data_size: clusters * 4096,
            extents: vec![(lcn * 4096, clusters * 4096)],
            allocated: clusters * 4096,
            compressed_or_sparse: false,
            resident_data: false,
            broken: false,
        }
    }

    fn run(entries: &[(u64, Entry)], patterns: &[&str]) -> MftScan {
        let map: HashMap<u64, Entry> = entries.iter().map(|(n, e)| (*n, clone_entry(e))).collect();
        let parsed = parse_patterns(&patterns.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let mut scan = MftScan::default();
        decide(&map, &parsed, 4096, &mut scan);
        scan.ranges.finalize();
        scan
    }

    fn clone_entry(e: &Entry) -> Entry {
        Entry {
            names: e.names.iter().map(|n| Name { parent: n.parent, name: n.name.clone() }).collect(),
            is_dir: e.is_dir,
            is_reparse: e.is_reparse,
            data_size: e.data_size,
            extents: e.extents.clone(),
            allocated: e.allocated,
            compressed_or_sparse: e.compressed_or_sparse,
            resident_data: e.resident_data,
            broken: e.broken,
        }
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
        shared.names.push(Name { parent: ROOT_RECORD, name: "keep-me.bin".into() });
        let scan = run(
            &[
                (20, dir(ROOT_RECORD, "Temp")),
                (30, shared),
            ],
            &["*.tmp"],
        );
        assert_eq!(scan.files_excluded, 0, "данные общие с уцелевшим именем — вычитать нельзя");
        assert_eq!(kept_reasons(&scan), vec![KeptReason::OtherNameKept]);
        assert!(scan.ranges.is_empty(), "ни один кластер не должен быть вычтен");
        // Запись из каталога всё равно убирается — под правило имя попало.
        assert_eq!(scan.paths.len(), 1);
    }

    /// Оба имени под исключением — данных не нужно никому.
    #[test]
    fn a_hard_link_excluded_under_every_name_is_subtracted() {
        let mut shared = file(20, "a.tmp", 100, 2);
        shared.names.push(Name { parent: 20, name: "b.tmp".into() });
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
        small.resident_data = true;
        small.data_size = 300;
        small.extents.clear();
        small.allocated = 0;
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
        torn.data_size = 100 * 4096; // заявлено 100 кластеров, найдено 2
        let scan = run(&[(30, torn)], &["*.tmp"]);
        assert_eq!(scan.files_excluded, 0);
        assert_eq!(kept_reasons(&scan), vec![KeptReason::ExtentsIncomplete]);
        assert!(scan.ranges.is_empty());
    }

    /// Файл без единого экстента, но не пустой — тот же случай, только грубее.
    #[test]
    fn a_file_with_no_extents_at_all_is_kept() {
        let mut hidden = file(ROOT_RECORD, "x.tmp", 0, 0);
        hidden.data_size = 954 * 1024 * 1024;
        hidden.extents.clear();
        hidden.allocated = 0;
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
        sparse.data_size = 64 * 4096; // 64 кластера «по размеру», выделено 2
        sparse.allocated = 2 * 4096;
        // Признак разреженности берётся у АТРИБУТА. Без него файл, занимающий
        // меньше своего размера, неотличим от недочитанного run list'а — и
        // должен считаться недочитанным, потому что ошибиться в эту сторону
        // безопаснее.
        sparse.compressed_or_sparse = true;
        let scan = run(&[(30, sparse)], &["*.tmp"]);
        assert_eq!(scan.files_excluded, 1, "выделенное кратно кластеру — файл сжат/разрежен");
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
}
