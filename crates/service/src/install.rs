//! Self install / uninstall of the backup service.
//!
//! One binary is both the service and its own installer:
//!
//! ```text
//! own-backup-service.exe /install      (Windows) → register with the SCM (auto-start, LocalSystem)
//! own-backup-service     --install     (Linux)   → write + enable a systemd unit
//! ```
//!
//! and the matching `/uninstall` / `--uninstall`. Both flag styles are accepted
//! on either platform (see `parse_mode`), but the work is platform-specific.
//! On Windows `/install` on a machine that already has the service re-points
//! the registration at the copy it was run from — that is all the setup
//! program (`packaging/windows/own-backup.nsi`) does about the service.
//! Registration needs Administrator (Windows) / root (Linux); the OS refuses
//! otherwise with a clear access-denied error that we surface verbatim.

use own_backup_common::tr;

// ---------------------------------------------------------------------------
// Windows: Service Control Manager
// ---------------------------------------------------------------------------

/// `CreateService` on a name the SCM already has.
#[cfg(target_os = "windows")]
const ERROR_SERVICE_EXISTS: i32 = 1073;

#[cfg(target_os = "windows")]
pub fn install() -> Result<(), Box<dyn std::error::Error>> {
    use std::ffi::OsString;
    use windows_service::service::{
        ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceType,
    };
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    let exe = std::env::current_exe()?;
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )?;

    let info = ServiceInfo {
        name: OsString::from(crate::SERVICE_NAME),
        display_name: OsString::from(crate::SERVICE_DISPLAY_NAME),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe.clone(),
        // No launch args: the SCM starts the exe with none, which maps to
        // Mode::Service → the Windows service dispatcher.
        launch_arguments: vec![],
        dependencies: vec![],
        // LocalSystem: the account that already has the privileges backups need
        // (VSS, raw device I/O) — and, being a service, runs without a UAC
        // prompt at scheduled time.
        account_name: None,
        account_password: None,
    };

    let access = ServiceAccess::CHANGE_CONFIG | ServiceAccess::START;
    let service = match manager.create_service(&info, access) {
        Ok(service) => service,
        // Already registered — by an earlier version, or by a copy run from
        // another folder. Point the registration at THIS executable instead of
        // failing: the installer runs `/install` after every copy, and removing
        // the service first only to create it again leaves it "marked for
        // deletion" for as long as anything (services.msc is enough) holds a
        // handle to it.
        Err(windows_service::Error::Winapi(error))
            if error.raw_os_error() == Some(ERROR_SERVICE_EXISTS) =>
        {
            let service = manager.open_service(crate::SERVICE_NAME, access)?;
            service.change_config(&info)?;
            service
        }
        Err(error) => return Err(error.into()),
    };
    service.set_description(crate::SERVICE_DESCRIPTION)?;

    // Start again after a crash. Without it a service that died — a panic, a
    // driver call that took it down — stayed stopped until the next reboot,
    // and scheduled copies stopped with it, silently (stand, 2026-10-18: a
    // killed service was found Stopped). A stop someone asked for is not a
    // failure and is left alone. Not worth failing the install over.
    {
        use std::time::Duration;
        use windows_service::service::{
            ServiceAction, ServiceActionType, ServiceFailureActions, ServiceFailureResetPeriod,
        };
        let restart = |secs| ServiceAction {
            action_type: ServiceActionType::Restart,
            delay: Duration::from_secs(secs),
        };
        if let Err(e) = service.update_failure_actions(ServiceFailureActions {
            reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 3600)),
            reboot_msg: None,
            command: None,
            actions: Some(vec![restart(10), restart(60), restart(300)]),
        }) {
            eprintln!("could not set the service to restart after a failure: {e}");
        }
    }

    println!(
        "{}",
        tr!(
            "install.win_installed",
            name = crate::SERVICE_DISPLAY_NAME,
            id = crate::SERVICE_NAME,
            path = exe.display()
        )
    );
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn uninstall() -> Result<(), Box<dyn std::error::Error>> {
    use std::time::Duration;
    use windows_service::service::{ServiceAccess, ServiceState};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(
        crate::SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
    )?;

    // Stop it first if running, so DeleteService takes effect now rather than
    // being deferred until the last handle closes / next reboot.
    if let Ok(status) = service.query_status() {
        if status.current_state != ServiceState::Stopped {
            let _ = service.stop();
            for _ in 0..20 {
                std::thread::sleep(Duration::from_millis(300));
                match service.query_status() {
                    Ok(s) if s.current_state == ServiceState::Stopped => break,
                    _ => {}
                }
            }
        }
    }

    service.delete()?;
    println!(
        "{}",
        tr!(
            "install.win_removed",
            name = crate::SERVICE_DISPLAY_NAME,
            id = crate::SERVICE_NAME
        )
    );
    Ok(())
}

#[cfg(target_os = "windows")]
fn open_service(
    access: windows_service::service::ServiceAccess,
) -> Result<windows_service::service::Service, Box<dyn std::error::Error>> {
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    Ok(manager.open_service(crate::SERVICE_NAME, access)?)
}

#[cfg(target_os = "windows")]
pub fn start() -> Result<(), Box<dyn std::error::Error>> {
    use windows_service::service::{ServiceAccess, ServiceState};
    let service = open_service(ServiceAccess::QUERY_STATUS | ServiceAccess::START)?;
    if service.query_status()?.current_state == ServiceState::Running {
        println!(
            "{}",
            tr!(
                "install.win_already_running",
                name = crate::SERVICE_DISPLAY_NAME
            )
        );
        return Ok(());
    }
    let no_args: [&std::ffi::OsStr; 0] = [];
    service.start(&no_args)?;
    println!(
        "{}",
        tr!("install.win_started", name = crate::SERVICE_DISPLAY_NAME)
    );
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn stop() -> Result<(), Box<dyn std::error::Error>> {
    use std::time::Duration;
    use windows_service::service::{ServiceAccess, ServiceState};
    let service = open_service(ServiceAccess::QUERY_STATUS | ServiceAccess::STOP)?;
    let state = service.query_status()?.current_state;
    if state == ServiceState::Stopped {
        println!(
            "{}",
            tr!(
                "install.win_already_stopped",
                name = crate::SERVICE_DISPLAY_NAME
            )
        );
        return Ok(());
    }
    if state != ServiceState::StopPending {
        service.stop()?;
    }
    // A durable consolidation can take far longer than six seconds. Never
    // report success (or let the updater copy the EXE) while it is draining.
    wait_until_stopped(
        || Ok(service.query_status()?.current_state == ServiceState::Stopped),
        Duration::from_secs(300),
    )?;
    println!(
        "{}",
        tr!("install.win_stopped", name = crate::SERVICE_DISPLAY_NAME)
    );
    Ok(())
}

#[cfg(any(windows, test))]
fn wait_until_stopped(
    mut stopped: impl FnMut() -> Result<bool, Box<dyn std::error::Error>>,
    timeout: std::time::Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if stopped()? {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                tr!(
                    "install.win_stop_timeout",
                    name = crate::SERVICE_DISPLAY_NAME,
                    seconds = timeout.as_secs_f64()
                )
                .into_owned(),
            )
            .into());
        }
        std::thread::sleep(remaining.min(std::time::Duration::from_millis(300)));
    }
}

#[cfg(target_os = "windows")]
pub fn status() -> Result<(), Box<dyn std::error::Error>> {
    use windows_service::service::ServiceAccess;
    let service = open_service(ServiceAccess::QUERY_STATUS)?;
    let s = service.query_status()?;
    println!(
        "{}",
        tr!(
            "install.win_status",
            name = crate::SERVICE_DISPLAY_NAME,
            id = crate::SERVICE_NAME,
            state = format!("{:?}", s.current_state)
        )
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Linux: the packaged systemd unit
// ---------------------------------------------------------------------------

/// Where the unit is, in the order systemd itself prefers them: an
/// administrator's copy in `/etc` over the package's in `/usr/lib`.
#[cfg(not(target_os = "windows"))]
fn linux_unit_candidates() -> [String; 3] {
    let name = crate::LINUX_UNIT_NAME;
    [
        format!("/etc/systemd/system/{name}.service"),
        format!("/usr/lib/systemd/system/{name}.service"),
        format!("/lib/systemd/system/{name}.service"),
    ]
}

/// The unit file systemd uses, if there is one.
#[cfg(not(target_os = "windows"))]
fn linux_unit_path() -> Option<String> {
    linux_unit_candidates()
        .into_iter()
        .find(|p| std::path::Path::new(p).is_file())
}

/// The description line of the unit older builds wrote into `/etc` from
/// `--install`. A unit in `/etc` shadows the packaged one, so such a leftover
/// would keep systemd running whatever binary that build was started from.
#[cfg(not(target_os = "windows"))]
fn legacy_unit_description() -> String {
    format!("Description={}", crate::SERVICE_DESCRIPTION)
}

/// Is this unit text one that `--install` generated before the package
/// shipped its own? Recognised by the exact description it was written with
/// and the `Type=simple` the packaged unit no longer uses; an administrator's
/// own unit is left alone.
#[cfg(not(target_os = "windows"))]
fn is_legacy_generated_unit(text: &str) -> bool {
    let description = legacy_unit_description();
    text.lines().any(|l| l.trim() == description) && text.lines().any(|l| l.trim() == "Type=simple")
}

/// Remove a unit an older `--install` left in `/etc`, so the packaged one is
/// in force. `Ok(Some(path))` when one was removed.
#[cfg(not(target_os = "windows"))]
fn remove_legacy_unit() -> Result<Option<String>, Box<dyn std::error::Error>> {
    remove_legacy_unit_in(std::path::Path::new("/etc/systemd/system"))
}

/// [`remove_legacy_unit`] for the unit directory `dir`. The `.wants` links
/// that the old `systemctl enable` made point at the removed file and go with
/// it — left behind, they would dangle, and `enable` of the packaged unit
/// stumbles over a link of the same name.
#[cfg(not(target_os = "windows"))]
fn remove_legacy_unit_in(
    dir: &std::path::Path,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let unit = dir.join(format!("{}.service", crate::LINUX_UNIT_NAME));
    let path = unit.display().to_string();
    let text = match std::fs::read_to_string(&unit) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {path}: {e}").into()),
    };
    if !is_legacy_generated_unit(&text) {
        return Ok(None);
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if !entry.file_name().to_string_lossy().ends_with(".wants") {
                continue;
            }
            let link = entry.path().join(unit.file_name().unwrap_or_default());
            if std::fs::read_link(&link).is_ok_and(|target| target == unit) {
                std::fs::remove_file(&link)
                    .map_err(|e| format!("cannot remove {}: {e}", link.display()))?;
            }
        }
    }
    std::fs::remove_file(&unit).map_err(|e| format!("cannot remove {path}: {e}"))?;
    Ok(Some(path))
}

/// The program an installed unit runs: the first word of its `ExecStart=`,
/// after systemd's `-@:+!` prefixes, unquoted.
#[cfg(not(target_os = "windows"))]
fn unit_exec_start(unit: &str) -> Option<std::path::PathBuf> {
    let line = unit
        .lines()
        .rev() // the last assignment is the one in force
        .find_map(|l| l.trim().strip_prefix("ExecStart="))?;
    let line = line.trim_start_matches(['-', '@', ':', '+', '!']);
    let program = if let Some(quoted) = line.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = quoted.chars();
        loop {
            match chars.next()? {
                '"' => break,
                '\\' => out.push(chars.next()?),
                c => out.push(c),
            }
        }
        out
    } else {
        line.split_whitespace().next()?.to_string()
    };
    (!program.is_empty()).then(|| std::path::PathBuf::from(program))
}

/// Enable and start the unit the package installed.
///
/// The unit, the access group (sysusers.d) and the directories (tmpfiles.d)
/// all come from the package now. This used to write its own unit into
/// `/etc` pointing at wherever it was run from, create the group and add
/// `$SUDO_USER` to it — none of which a package manager can undo, and the
/// unit in `/etc` silently overrode the packaged one. Who gets access is
/// `--grant-access`'s question.
#[cfg(not(target_os = "windows"))]
pub fn install() -> Result<(), Box<dyn std::error::Error>> {
    if let Some(removed) = remove_legacy_unit()? {
        println!(
            "{}",
            tr!("install.linux_legacy_unit_removed", path = removed)
        );
    }
    let Some(unit_path) = linux_unit_path() else {
        return Err(tr!("install.linux_no_unit", unit = crate::LINUX_UNIT_NAME)
            .into_owned()
            .into());
    };
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", crate::LINUX_UNIT_NAME])?;
    println!(
        "{}",
        tr!(
            "install.linux_installed",
            path = unit_path,
            unit = crate::LINUX_UNIT_NAME
        )
    );
    Ok(())
}

/// Give a person access to the engine: membership in the access group.
///
/// Run as root, by `pkexec` (the polkit action `com.ownbackup.grant-access`
/// lets the window ask for it), by `sudo`, or by hand with a user name. The
/// package cannot do it at install time: under PackageKit or unattended
/// upgrades there is no `SUDO_USER` to say who the person is.
#[cfg(not(target_os = "windows"))]
pub fn grant_access(user: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let group = own_backup_common::UNIX_ACCESS_GROUP;
    let user = match user {
        Some(user) => user.to_string(),
        None => grant_target_user().ok_or_else(|| tr!("install.linux_no_user").into_owned())?,
    };
    if user == "root" {
        return Err(tr!("install.linux_root_needs_no_group").into_owned().into());
    }
    add_user_to_group(&user, group).map_err(|e| {
        tr!(
            "install.linux_user_not_added",
            user = user,
            group = group,
            error = e
        )
        .into_owned()
    })?;
    println!(
        "{}",
        tr!("install.linux_user_added", user = user, group = group)
    );
    Ok(())
}

/// Who `--grant-access` without a name means: the user `pkexec` was asked by
/// (`PKEXEC_UID`), else the one behind `sudo`. Never `$USER`: under both it
/// is root.
#[cfg(not(target_os = "windows"))]
fn grant_target_user() -> Option<String> {
    if let Some(uid) = std::env::var("PKEXEC_UID")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
    {
        return user_name_of(uid);
    }
    std::env::var("SUDO_USER")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty() && v != "root")
}

#[cfg(not(target_os = "windows"))]
fn user_name_of(uid: u32) -> Option<String> {
    // SAFETY: getpwuid returns a pointer into a static buffer (read at once,
    // not kept) or null when there is no such user.
    let pw = unsafe { libc::getpwuid(uid) };
    if pw.is_null() {
        return None;
    }
    // SAFETY: non-null pw points at a valid passwd whose pw_name is a C string.
    let name = unsafe { std::ffi::CStr::from_ptr((*pw).pw_name) };
    name.to_str().ok().map(str::to_string)
}

#[cfg(not(target_os = "windows"))]
fn add_user_to_group(user: &str, group: &str) -> Result<(), Box<dyn std::error::Error>> {
    let status = std::process::Command::new("usermod")
        .args(["-aG", group, user])
        .status()
        .map_err(|e| format!("cannot run usermod: {e}"))?;
    if !status.success() {
        return Err(format!("usermod exited {status}").into());
    }
    Ok(())
}

/// Stop and disable the unit. The unit file belongs to the package and is
/// removed with it; only a unit an older `--install` wrote is removed here.
#[cfg(not(target_os = "windows"))]
pub fn uninstall() -> Result<(), Box<dyn std::error::Error>> {
    // `disable --now` waits for the stop, consolidation included
    // (TimeoutStopSec in the unit).
    let disabled = systemctl(&["disable", "--now", crate::LINUX_UNIT_NAME]);
    if let Some(removed) = remove_legacy_unit()? {
        println!(
            "{}",
            tr!("install.linux_legacy_unit_removed", path = removed)
        );
        let _ = systemctl(&["daemon-reload"]);
    }
    disabled?;
    println!(
        "{}",
        tr!("install.linux_removed", unit = crate::LINUX_UNIT_NAME)
    );
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub fn start() -> Result<(), Box<dyn std::error::Error>> {
    systemctl(&["start", crate::LINUX_UNIT_NAME])?;
    println!(
        "{}",
        tr!("install.linux_started", unit = crate::LINUX_UNIT_NAME)
    );
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub fn stop() -> Result<(), Box<dyn std::error::Error>> {
    systemctl(&["stop", crate::LINUX_UNIT_NAME])?;
    println!(
        "{}",
        tr!("install.linux_stopped", unit = crate::LINUX_UNIT_NAME)
    );
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub fn status() -> Result<(), Box<dyn std::error::Error>> {
    // `systemctl status` prints its own report to stdout/stderr; ignore the
    // non-zero exit it returns when the unit is stopped.
    let _ = std::process::Command::new("systemctl")
        .args(["status", "--no-pager", crate::LINUX_UNIT_NAME])
        .status();
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn systemctl(args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    let status = std::process::Command::new("systemctl")
        .args(args)
        .status()?;
    if !status.success() {
        return Err(format!("`systemctl {}` failed ({status})", args.join(" ")).into());
    }
    Ok(())
}

/// Stop the service and start it again. Stop failures (e.g. already stopped) are
/// tolerated so a restart still (re)starts the service.
pub fn restart() -> Result<(), Box<dyn std::error::Error>> {
    let _ = stop();
    std::thread::sleep(std::time::Duration::from_millis(500));
    start()
}

/// Delete everything the service keeps on this machine: settings, jobs, run
/// history and the remembered volume keys. Backups are somewhere else and are
/// not this command's to touch.
///
/// This is the uninstaller's «удалить также настройки», and it is here rather
/// than in the uninstaller for a reason found by the first run that answered
/// yes (2026-10-06): the key store, `.own-backup-service-keys`, admits
/// LocalSystem alone. An administrator's ordinary recursive delete removed the
/// settings and the jobs and left exactly the keys behind.
///
/// Only the DEFAULT data folder goes. A `data_dir` written into `config.toml`
/// by hand may be anything, a drive root included, and "delete the settings"
/// must never be able to mean "delete D:\".
pub fn purge_data() -> Result<(), Box<dyn std::error::Error>> {
    let dir = own_backup_common::config::AppConfig::default().data_dir;

    // A running service would write half of it back, and would be left without
    // the key it is using.
    #[cfg(target_os = "windows")]
    {
        use windows_service::service::{ServiceAccess, ServiceState};
        if let Ok(service) = open_service(ServiceAccess::QUERY_STATUS) {
            if service.query_status()?.current_state != ServiceState::Stopped {
                return Err(
                    tr!("install.purge_running", name = crate::SERVICE_DISPLAY_NAME)
                        .into_owned()
                        .into(),
                );
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    if linux_unit_running() {
        return Err(
            tr!("install.purge_running", name = crate::SERVICE_DISPLAY_NAME)
                .into_owned()
                .into(),
        );
    }

    if !dir.exists() {
        println!("{}", tr!("install.purge_nothing", path = dir.display()));
        return Ok(());
    }

    // An engine started by hand (`--run`, `--foreground`) is no unit of
    // systemd's, but it holds the data directory's lock for as long as it
    // runs. Holding it here also keeps one from starting mid-delete.
    #[cfg(not(target_os = "windows"))]
    let _ownership = crate::single_instance::claim(&dir).map_err(|e| match e {
        crate::single_instance::Taken::AlreadyRunning => {
            tr!("install.purge_running", name = crate::SERVICE_DISPLAY_NAME).into_owned()
        }
        other => other.to_string(),
    })?;

    #[cfg(target_os = "windows")]
    {
        let (_, failed) =
            own_backup_backup::true_removal_win::delete_folder_with_backup_rights(&dir);
        if failed > 0 || dir.exists() {
            return Err(
                tr!("install.purge_failed", path = dir.display(), count = failed)
                    .into_owned()
                    .into(),
            );
        }
    }
    #[cfg(not(target_os = "windows"))]
    std::fs::remove_dir_all(&dir)?;

    println!("{}", tr!("install.purge_done", path = dir.display()));
    Ok(())
}

/// Put THIS executable in place of the installed one: stop, replace, start.
///
/// A running service holds its own file open, so a new build cannot be copied
/// over it — which turns every update into three manual steps. Skipping them is
/// silent: the new file sits beside the old one, the service keeps running the
/// old code, and the next run reproduces the very bug that was just fixed.
/// That happened on 2026-09-22 and cost a full test cycle.
///
/// Run it from the NEW binary, wherever it was put:
/// `own-backup-service.exe.new /update`.
pub fn update_from_here() -> Result<(), Box<dyn std::error::Error>> {
    let here = std::env::current_exe()?;
    let installed = installed_binary()?;

    // Same file: nothing to do, and copying a file onto itself would truncate
    // it. This is the ordinary answer when somebody runs `/update` twice.
    let same = std::fs::canonicalize(&here).ok() == std::fs::canonicalize(&installed).ok();
    if same {
        println!(
            "{}",
            tr!("install.update_same_file", path = installed.display())
        );
        return Ok(());
    }

    println!(
        "{}",
        tr!("install.update_installed", path = installed.display())
    );
    println!("{}", tr!("install.update_replacing", path = here.display()));
    stop()?;
    // The SCM reports Stopped before the process has necessarily exited, and a
    // process that still holds its image file makes the copy fail with a
    // sharing violation. Retry briefly rather than demanding a second attempt.
    let mut last: Option<std::io::Error> = None;
    for attempt in 0..20 {
        match std::fs::copy(&here, &installed) {
            Ok(_) => {
                last = None;
                break;
            }
            Err(e) => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(300));
                let _ = attempt;
            }
        }
    }
    if let Some(e) = last {
        // The service is stopped and the binary is untouched: start it again so
        // the machine is not left without one because an update failed.
        let _ = start();
        return Err(tr!(
            "install.update_failed",
            path = installed.display(),
            error = e
        )
        .into_owned()
        .into());
    }
    println!("{}", tr!("install.update_done"));
    start()?;
    // Leaving the staged copy would make the next `/update` a no-op against a
    // stale file; it has done its job.
    if here
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("new"))
    {
        let _ = std::fs::remove_file(&here);
    }
    Ok(())
}

/// Where the installed service's binary is, as the system itself records it.
///
/// Asked of the SCM rather than guessed from this executable's folder: an
/// update may be staged anywhere, and replacing the wrong file would leave the
/// machine running the old service while looking like it had been updated.
#[cfg(target_os = "windows")]
fn installed_binary() -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    use windows_service::service::ServiceAccess;
    let service = open_service(ServiceAccess::QUERY_CONFIG)?;
    Ok(service.query_config()?.executable_path)
}

#[cfg(test)]
mod stop_tests {
    use super::wait_until_stopped;
    use std::time::Duration;

    #[test]
    fn a_service_that_keeps_running_is_a_timeout_not_success() {
        let error = wait_until_stopped(|| Ok(false), Duration::from_millis(20)).unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn stop_waits_for_the_observed_state_transition() {
        let mut observations = 0;
        wait_until_stopped(
            || {
                observations += 1;
                Ok(observations == 3)
            },
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(observations, 3);
    }

    #[test]
    fn an_unreadable_service_status_is_propagated() {
        let error = wait_until_stopped(
            || Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied).into()),
            Duration::from_secs(2),
        )
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }
}

/// Where the installed service's binary is: the program the unit in force
/// runs (`ExecStart`). It used to be a fixed `/usr/local/bin/own-backup-service`
/// while the unit named whatever path it was installed from — so `--update`
/// replaced a file nothing ran, restarted the old one and said "replaced".
#[cfg(not(target_os = "windows"))]
fn installed_binary() -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    let Some(unit_path) = linux_unit_path() else {
        return Err(tr!("install.linux_no_unit", unit = crate::LINUX_UNIT_NAME)
            .into_owned()
            .into());
    };
    let unit = std::fs::read_to_string(&unit_path).map_err(|e| {
        tr!("install.linux_unit_unreadable", path = unit_path, error = e).into_owned()
    })?;
    unit_exec_start(&unit).ok_or_else(|| {
        tr!("install.linux_unit_no_exec", path = unit_path)
            .into_owned()
            .into()
    })
}

/// Is the unit running (or starting, or stopping)? `systemctl is-active`
/// prints the state even when it exits non-zero; no systemctl at all is
/// "not a unit of ours running".
#[cfg(not(target_os = "windows"))]
fn linux_unit_running() -> bool {
    std::process::Command::new("systemctl")
        .args(["is-active", crate::LINUX_UNIT_NAME])
        .output()
        .map(|o| {
            matches!(
                String::from_utf8_lossy(&o.stdout).trim(),
                "active" | "activating" | "deactivating" | "reloading" | "refreshing"
            )
        })
        .unwrap_or(false)
}

#[cfg(all(test, not(target_os = "windows")))]
mod linux_unit_tests {
    use super::*;

    const PACKAGED_UNIT: &str = include_str!("../../../packaging/linux/own-backup.service");

    /// The unit the package ships, checked here because nothing else reads
    /// it before it is on a machine.
    #[test]
    fn the_packaged_unit_waits_for_a_long_stop_and_for_the_network() {
        for line in [
            "Type=notify",
            "NotifyAccess=main",
            "ExecStart=/usr/libexec/own-backup/own-backup-service --run",
            "TimeoutStopSec=330",
            "RestartSec=10",
            "KillMode=mixed",
            "Wants=network-online.target",
            "After=network-online.target remote-fs.target",
            "StateDirectory=own-backup",
            "LogsDirectory=own-backup",
            "RuntimeDirectory=own-backup",
            // The freeze markers in /run/own-backup must outlive a crash.
            "RuntimeDirectoryPreserve=yes",
        ] {
            assert!(
                PACKAGED_UNIT.lines().any(|l| l == line),
                "{line} missing:\n{PACKAGED_UNIT}"
            );
        }
    }

    /// Any of these gives the service a mount namespace of its own, where
    /// the images it mounts and the read-only remount of a busy ESP would
    /// stay invisible to the rest of the system.
    #[test]
    fn the_packaged_unit_shares_the_mount_namespace() {
        for option in [
            "PrivateTmp=",
            "PrivateDevices=",
            "PrivateMounts=",
            "ProtectSystem=",
            "ProtectHome=",
            "ProtectKernelTunables=",
            "ProtectKernelModules=",
            "ProtectKernelLogs=",
            "ProtectControlGroups=",
            "ProtectClock=",
            "ReadOnlyPaths=",
            "ReadWritePaths=",
            "InaccessiblePaths=",
            "TemporaryFileSystem=",
            "MountFlags=",
        ] {
            assert!(
                !PACKAGED_UNIT
                    .lines()
                    .any(|l| l.trim_start().starts_with(option)),
                "{option} would give the engine a private mount namespace"
            );
        }
    }

    #[test]
    fn update_reads_back_the_program_the_packaged_unit_runs() {
        assert_eq!(
            unit_exec_start(PACKAGED_UNIT),
            Some(std::path::PathBuf::from(
                "/usr/libexec/own-backup/own-backup-service"
            ))
        );
    }

    #[test]
    fn exec_start_is_read_the_way_systemd_reads_it() {
        let read = |unit: &str| unit_exec_start(unit).map(|p| p.display().to_string());
        assert_eq!(
            read("[Service]\nExecStart=-/usr/local/bin/x --run\n").as_deref(),
            Some("/usr/local/bin/x")
        );
        assert_eq!(
            read("ExecStart=\"/opt/Own Backup/own-backup-service\" --run\n").as_deref(),
            Some("/opt/Own Backup/own-backup-service")
        );
        assert_eq!(
            read("ExecStart=\"/opt/odd \\\"name\\\"\\\\dir/x\" --run\n").as_deref(),
            Some("/opt/odd \"name\"\\dir/x")
        );
        // The last assignment is the one in force.
        assert_eq!(
            read("ExecStart=/a --run\nExecStart=/b --run\n").as_deref(),
            Some("/b")
        );
        assert_eq!(read("[Service]\nType=simple\n"), None);
        assert_eq!(read("ExecStart=\n"), None);
        assert_eq!(read("ExecStart=\"/unterminated\n"), None);
    }

    /// What an older `--install` wrote, byte for byte, is recognised and
    /// removed; an administrator's own unit, or the packaged one, is not.
    #[test]
    fn only_a_unit_an_older_install_generated_counts_as_legacy() {
        let legacy = format!(
            "[Unit]\n{}\nWants=network-online.target\n\n[Service]\nType=simple\n\
             ExecStart=/usr/local/bin/own-backup-service --run\n",
            legacy_unit_description()
        );
        assert!(is_legacy_generated_unit(&legacy));
        assert!(!is_legacy_generated_unit(PACKAGED_UNIT));
        let admins = "[Unit]\nDescription=my own engine\n[Service]\nType=simple\n";
        assert!(!is_legacy_generated_unit(admins));
    }

    #[test]
    fn a_legacy_unit_goes_with_its_wants_link_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let unit = dir.path().join("own-backup.service");
        std::fs::write(
            &unit,
            format!(
                "[Unit]\n{}\n[Service]\nType=simple\nExecStart=/x --run\n\
                 [Install]\nWantedBy=multi-user.target\n",
                legacy_unit_description()
            ),
        )
        .unwrap();
        let wants = dir.path().join("multi-user.target.wants");
        std::fs::create_dir(&wants).unwrap();
        std::os::unix::fs::symlink(&unit, wants.join("own-backup.service")).unwrap();
        // Someone else's link of the same name, to the packaged unit.
        let other = dir.path().join("graphical.target.wants");
        std::fs::create_dir(&other).unwrap();
        std::os::unix::fs::symlink(
            "/usr/lib/systemd/system/own-backup.service",
            other.join("own-backup.service"),
        )
        .unwrap();

        let removed = remove_legacy_unit_in(dir.path()).unwrap();
        assert_eq!(removed.as_deref(), Some(unit.to_str().unwrap()));
        assert!(!unit.exists());
        assert!(wants.join("own-backup.service").symlink_metadata().is_err());
        assert!(other.join("own-backup.service").symlink_metadata().is_ok());
        // Nothing left to remove the second time.
        assert_eq!(remove_legacy_unit_in(dir.path()).unwrap(), None);
    }

    #[test]
    fn an_administrators_unit_in_etc_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let unit = dir.path().join("own-backup.service");
        std::fs::write(&unit, "[Service]\nType=simple\nExecStart=/mine\n").unwrap();
        assert_eq!(remove_legacy_unit_in(dir.path()).unwrap(), None);
        assert!(unit.exists());
    }
}
