//! What a copy from the command line shares with the copy a job makes: which
//! version it is written over, and the check afterwards.
//!
//! A job gets both from the service. The run planner picks the parent out of
//! the run HISTORY (`own_backup_core::plan_run` — the last successful run of
//! that job), and `validate_after` runs the verification once the copy is
//! closed. From the command line there is no history, so the parent is found
//! the only other way it can be: among the images already lying in the
//! destination folder, grouped into chains by their own sidecars
//! ([`own_backup_backup::chains`]). The check is the same call the job option
//! makes, so "проверить" means one thing everywhere.
//!
//! Only images that belong to NO job are considered — a hand-made copy must
//! never end up as the parent of a scheduled job's increment, nor the other
//! way round.

use own_backup_common::types::{BackupType, ImageFormat};
use own_backup_common::{OwnBackupError, Result};
use std::path::{Path, PathBuf};

/// `full | incremental | differential`, as it is typed.
pub fn backup_kind(word: &str) -> Result<BackupType> {
    match word.to_ascii_lowercase().as_str() {
        "full" => Ok(BackupType::Full),
        "incremental" | "inc" => Ok(BackupType::Incremental),
        "differential" | "diff" => Ok(BackupType::Differential),
        other => Err(OwnBackupError::Config(format!(
            "неизвестный вид копии «{other}» — ожидается full, incremental или differential"
        ))),
    }
}

/// The image a child copy is written over, and the kind it can actually be.
///
/// `explicit` (`--parent`) wins and is only checked for existence. `auto`
/// (`--parent-auto`) looks beside `output` for the newest chain of
/// command-line images in the same container format: its newest image for an
/// incremental, its full for a differential. Finding nothing is not an error —
/// it means there is nothing to be a child of, so the answer is a full copy,
/// said out loud. That is what the service's planner does with a chain it
/// cannot use, and the engine falls back the same way on a parent it cannot
/// read.
pub fn resolve_parent(
    kind: BackupType,
    explicit: Option<&str>,
    auto: bool,
    output: &Path,
    format: ImageFormat,
) -> Result<(BackupType, Option<PathBuf>)> {
    if kind == BackupType::Full {
        return Ok((kind, None));
    }
    if let Some(p) = explicit {
        let path = PathBuf::from(p);
        if !path.exists() {
            return Err(OwnBackupError::Config(format!(
                "родительский образ не найден: {}",
                path.display()
            )));
        }
        return Ok((kind, Some(path)));
    }
    if !auto {
        return Err(OwnBackupError::Config(
            "инкрементальной и дифференциальной копии нужен родитель: укажите --parent ОБРАЗ \
             или --parent-auto, чтобы взять его из цепочки рядом с --output"
                .into(),
        ));
    }

    let folder = output.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let chains = own_backup_backup::chains::list_chains(folder, None)?;
    let newest = chains
        .iter()
        .filter(|c| {
            c.images.iter().all(|i| i.container.eq_ignore_ascii_case(format.extension()))
        })
        .max_by(|a, b| a.newest_created_at().cmp(b.newest_created_at()));
    let Some(chain) = newest else {
        println!(
            "Родителя рядом с {} нет — пишем полную копию.",
            folder.display()
        );
        return Ok((BackupType::Full, None));
    };
    let pick = match kind {
        BackupType::Incremental => chain.images.last(),
        _ => chain.images.first(),
    };
    let Some(image) = pick else {
        println!("Цепочка рядом с {} пуста — пишем полную копию.", folder.display());
        return Ok((BackupType::Full, None));
    };
    println!(
        "Родитель из цепочки: {} (в цепочке версий: {})",
        image.image_path.display(),
        chain.images.len()
    );
    Ok((kind, Some(image.image_path.clone())))
}

/// The check the job option «проверять после создания» runs: every block of
/// the finished image against the checksums its own sidecar carries.
#[cfg(target_os = "windows")]
pub fn verify_after(image: &Path) -> Result<()> {
    use std::io::Write;

    println!("Проверка образа по контрольным суммам…");
    let mut last = u64::MAX;
    let summary = own_backup_backup::engine_win::verify_image(image, |done, total| {
        let percent = (done * 100).checked_div(total).unwrap_or(0);
        if percent != last {
            last = percent;
            print!("\r  {percent:3}%  {done} / {total} блоков   ");
            let _ = std::io::stdout().flush();
        }
        Ok(())
    })?;
    println!();
    if summary.valid() {
        println!(
            "  Проверено: {} блоков за {:.1} с — все совпали.",
            summary.blocks_checked,
            summary.duration.as_secs_f64()
        );
        return Ok(());
    }
    // A copy that does not match its own checksums is not a copy. Said as an
    // error, so a script that made it stops instead of reporting success.
    println!(
        "  НЕ СОВПАЛО: {} блоков из {} — {:?}",
        summary.mismatched_blocks.len(),
        summary.blocks_checked,
        &summary.mismatched_blocks[..summary.mismatched_blocks.len().min(16)]
    );
    Err(OwnBackupError::ImageFormat(format!(
        "{}: образ не совпадает со своими контрольными суммами",
        image.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kind_is_read_the_way_people_type_it() {
        assert_eq!(backup_kind("full").unwrap(), BackupType::Full);
        assert_eq!(backup_kind("FULL").unwrap(), BackupType::Full);
        assert_eq!(backup_kind("inc").unwrap(), BackupType::Incremental);
        assert_eq!(backup_kind("incremental").unwrap(), BackupType::Incremental);
        assert_eq!(backup_kind("diff").unwrap(), BackupType::Differential);
        assert_eq!(backup_kind("differential").unwrap(), BackupType::Differential);
        assert!(backup_kind("weekly").is_err());
    }

    /// A full copy has no parent to look for, whatever the flags say.
    #[test]
    fn a_full_copy_never_looks_for_a_parent() {
        let (kind, parent) = resolve_parent(
            BackupType::Full,
            Some("nonexistent.vhdx"),
            true,
            Path::new("/tmp/out.vhdx"),
            ImageFormat::Vhdx,
        )
        .unwrap();
        assert_eq!(kind, BackupType::Full);
        assert_eq!(parent, None);
    }

    /// A child copy with neither a named parent nor permission to find one is
    /// a request that cannot be answered — said before anything is copied.
    #[test]
    fn a_child_without_a_parent_is_refused_up_front() {
        let e = resolve_parent(
            BackupType::Incremental,
            None,
            false,
            Path::new("/tmp/out.vhdx"),
            ImageFormat::Vhdx,
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("--parent"), "{e}");
    }

    /// A named parent that is not there is refused too: the engine would fall
    /// back to a full copy, and a script asking for an increment deserves to
    /// hear that it cannot have one.
    #[test]
    fn a_named_parent_that_is_missing_is_refused() {
        let e = resolve_parent(
            BackupType::Differential,
            Some("/nonexistent/parent.vhdx"),
            false,
            Path::new("/tmp/out.vhdx"),
            ImageFormat::Vhdx,
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("не найден"), "{e}");
    }

    /// An empty folder holds no chain, and that is an answer, not a failure:
    /// the copy becomes a full one.
    #[test]
    fn nothing_to_be_a_child_of_means_a_full_copy() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.vhdx");
        let (kind, parent) =
            resolve_parent(BackupType::Incremental, None, true, &out, ImageFormat::Vhdx).unwrap();
        assert_eq!(kind, BackupType::Full);
        assert_eq!(parent, None);
    }
}
