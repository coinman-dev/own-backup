//! Settings changed through the service (`GetSettings` / `SaveSettings`).
//!
//! On Linux `config.toml` lives in root's data folder (0750, the access
//! group), so the window can read it but not write it; it asks here instead,
//! and the service writes the file under the same lock and atomic replace
//! every writer uses (`AppConfig::update_file`). The window on Windows still
//! writes `C:\ProgramData\OwnBackup\config.toml` itself, as it always has; the
//! call answers there too, but nothing on Windows uses it.
//!
//! One field is more than a preference. `log_mirror_dir` names a folder in
//! which the service — root — creates a file and appends to it for as long as
//! it runs, so a client that may change settings must not be able to point it
//! at `/etc`. The rule, on Unix:
//!
//! * an absolute path, without `.` or `..`;
//! * no symbolic link anywhere along it, and the end an existing directory —
//!   a stick that is not plugged in is named once it is;
//! * unless the caller is root: a directory the caller owns, i.e. one they
//!   could create that file in themselves (their home, a stick the desktop
//!   mounted for them). A caller whose identity is not known (the loopback
//!   TCP of a development run) may only switch the mirror off.
//!
//! Checking a path once is not enough on its own: the owner of a folder can
//! swap it for a link afterwards. The writer (`common::log_mirror`) therefore
//! opens it the same way, refusing a link at every step and a file that is
//! a second name of another one.

use std::path::Path;

use own_backup_common::config::{AppConfig, SettingsPatch};
use own_backup_common::tr;
use tonic::{Request, Status};

/// Who is asking, as far as the transport can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Caller {
    /// Root over the Unix socket. On Windows every caller: the console user
    /// there writes `config.toml` directly anyway, so asking the service gives
    /// them nothing they do not already have.
    Unrestricted,
    /// A user over the Unix socket, by uid (`SO_PEERCRED`).
    #[cfg_attr(not(unix), allow(dead_code))]
    User(u32),
    /// No credentials: the loopback TCP of a `--foreground` development run.
    #[cfg_attr(not(unix), allow(dead_code))]
    Unknown,
}

/// The caller of `req`.
pub(crate) fn caller_of<T>(req: &Request<T>) -> Caller {
    #[cfg(unix)]
    {
        let cred = req
            .extensions()
            .get::<tonic::transport::server::UdsConnectInfo>()
            .and_then(|info| info.peer_cred);
        match cred {
            Some(c) if c.uid() == 0 => Caller::Unrestricted,
            Some(c) => Caller::User(c.uid()),
            None => Caller::Unknown,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = req;
        Caller::Unrestricted
    }
}

/// Apply `patch` to the config at `path` for `caller`, and return what is now
/// stored. The language takes effect for this process at once rather than at
/// the next re-read of the file.
pub(crate) fn save(
    path: &Path,
    patch: &SettingsPatch,
    caller: Caller,
) -> Result<AppConfig, Status> {
    if let Some(dir) = &patch.log_mirror_dir {
        check_mirror_dir(dir, caller)?;
    }
    let saved = AppConfig::update_file(path, |config| patch.apply(config))
        .map_err(|e| Status::internal(tr!("settings.not_saved", error = e).into_owned()))?;
    if patch.ui_language.is_some() {
        own_backup_common::lang::set(&saved.ui_language);
    }
    tracing::info!(
        level = ?patch.log_level,
        mirror = ?patch.log_mirror_dir,
        compression = ?patch.default_compression_level,
        remembered = patch.remembered_backups.as_ref().map(Vec::len),
        language = ?patch.ui_language,
        ?caller,
        "settings changed"
    );
    Ok(saved)
}

/// Whether `caller` may have root write its log mirror into `dir`; see the
/// module note for the rule. Empty switches the mirror off and is always
/// allowed.
pub(crate) fn check_mirror_dir(dir: &str, caller: Caller) -> Result<(), Status> {
    let dir = dir.trim();
    if dir.is_empty() {
        return Ok(());
    }
    let path = Path::new(dir);
    if !path.is_absolute()
        || path.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
    {
        return Err(Status::invalid_argument(
            tr!("settings.mirror_not_absolute", path = dir).into_owned(),
        ));
    }
    #[cfg(unix)]
    {
        check_mirror_dir_unix(path, dir, caller)
    }
    #[cfg(not(unix))]
    {
        let _ = caller;
        Ok(())
    }
}

#[cfg(unix)]
fn check_mirror_dir_unix(path: &Path, shown: &str, caller: Caller) -> Result<(), Status> {
    use std::os::unix::fs::MetadataExt;

    if caller == Caller::Unknown {
        return Err(Status::permission_denied(
            tr!("settings.mirror_unknown_caller").into_owned(),
        ));
    }
    // Every step looked at itself, not followed: a link anywhere means the
    // folder is not where the path says it is.
    let mut at = std::path::PathBuf::new();
    let mut last = None;
    for part in path.components() {
        at.push(part);
        if part == std::path::Component::RootDir {
            continue;
        }
        let meta = match std::fs::symlink_metadata(&at) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Status::failed_precondition(
                    tr!("settings.mirror_missing", path = shown).into_owned(),
                ))
            }
            Err(e) => {
                return Err(Status::failed_precondition(
                    tr!("settings.mirror_unreadable", path = shown, error = e).into_owned(),
                ))
            }
        };
        if meta.file_type().is_symlink() {
            return Err(Status::invalid_argument(
                tr!("settings.mirror_symlink", path = shown, link = at.display()).into_owned(),
            ));
        }
        if !meta.is_dir() {
            return Err(Status::invalid_argument(
                tr!("settings.mirror_not_dir", path = shown).into_owned(),
            ));
        }
        last = Some(meta);
    }
    match (caller, last) {
        (Caller::Unrestricted, _) => Ok(()),
        (Caller::User(uid), Some(meta)) if meta.uid() == uid => Ok(()),
        // `/` itself, or a folder of someone else's.
        _ => Err(Status::permission_denied(
            tr!("settings.mirror_not_yours", path = shown).into_owned(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switching_the_mirror_off_is_always_allowed() {
        assert!(check_mirror_dir("", Caller::Unknown).is_ok());
        assert!(check_mirror_dir("  ", Caller::User(1000)).is_ok());
    }

    #[test]
    fn a_relative_or_dotted_path_is_refused() {
        for bad in ["logs", "./logs", "/tmp/../etc"] {
            let e = check_mirror_dir(bad, Caller::Unrestricted).unwrap_err();
            assert_eq!(e.code(), tonic::Code::InvalidArgument, "{bad}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_user_may_name_only_a_folder_of_their_own_without_links() {
        use std::os::unix::fs::MetadataExt;

        let home = tempfile::tempdir().unwrap();
        let mine = home.path().join("logs");
        std::fs::create_dir(&mine).unwrap();
        let me = std::fs::metadata(&mine).unwrap().uid();
        let text = mine.display().to_string();

        assert!(check_mirror_dir(&text, Caller::User(me)).is_ok());
        assert!(check_mirror_dir(&text, Caller::Unrestricted).is_ok());
        let e = check_mirror_dir(&text, Caller::User(me.wrapping_add(1))).unwrap_err();
        assert_eq!(e.code(), tonic::Code::PermissionDenied);
        let e = check_mirror_dir(&text, Caller::Unknown).unwrap_err();
        assert_eq!(e.code(), tonic::Code::PermissionDenied);

        // `/etc` belongs to root, not to whoever runs this test — unless that
        // is root, for whom it is allowed.
        if me != 0 {
            let e = check_mirror_dir("/etc", Caller::User(me)).unwrap_err();
            assert_eq!(e.code(), tonic::Code::PermissionDenied);
        }

        let link = home.path().join("link");
        std::os::unix::fs::symlink(&mine, &link).unwrap();
        let e = check_mirror_dir(&link.display().to_string(), Caller::User(me)).unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
        let e = check_mirror_dir(&link.join("x").display().to_string(), Caller::Unrestricted)
            .unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);

        let missing = home.path().join("stick").display().to_string();
        let e = check_mirror_dir(&missing, Caller::User(me)).unwrap_err();
        assert_eq!(e.code(), tonic::Code::FailedPrecondition);

        let file = home.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        let e = check_mirror_dir(&file.display().to_string(), Caller::User(me)).unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument);
    }

    #[cfg(unix)]
    #[test]
    fn a_refused_mirror_leaves_the_file_as_it_was_and_the_rest_is_saved() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("config.toml");
        let refused = SettingsPatch {
            log_level: Some("debug".into()),
            log_mirror_dir: Some("/etc".into()),
            ..Default::default()
        };
        assert!(save(&path, &refused, Caller::Unknown).is_err());
        assert!(!path.exists(), "nothing is written when the check fails");

        let fine = SettingsPatch {
            log_level: Some("debug".into()),
            remembered_backups: Some(vec!["/mnt/b/one.qcow2".into()]),
            ..Default::default()
        };
        let saved = save(&path, &fine, Caller::Unknown).unwrap();
        assert_eq!(saved.log_level, "debug");
        let back = AppConfig::load_from_file(&path).unwrap();
        assert_eq!(
            back.remembered_backups,
            vec!["/mnt/b/one.qcow2".to_string()]
        );
        assert_eq!(back.log_mirror_dir, "");
    }
}
