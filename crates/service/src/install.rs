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
// Linux: systemd unit
// ---------------------------------------------------------------------------

#[cfg(not(target_os = "windows"))]
pub fn install() -> Result<(), Box<dyn std::error::Error>> {
    let exe = std::env::current_exe()?;
    let unit_path = format!("/etc/systemd/system/{}.service", crate::LINUX_UNIT_NAME);
    let unit = format!(
        "[Unit]\n\
         Description={desc}\n\
         After=network.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exe} --run\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        desc = crate::SERVICE_DESCRIPTION,
        exe = exe.display(),
    );

    // Access group so the GUI/CLI reach the service's UDS without root (the
    // Unix analogue of the named-pipe DACL). Create it and add the human who
    // ran `sudo … --install` (SUDO_USER), else the current user.
    let group = own_backup_common::UNIX_ACCESS_GROUP;
    ensure_group(group)?;
    let member = install_target_user();
    let mut group_note = String::new();
    if let Some(user) = &member {
        match add_user_to_group(user, group) {
            Ok(()) => {
                group_note =
                    tr!("install.linux_user_added", user = user, group = group).into_owned();
            }
            Err(e) => {
                group_note = tr!(
                    "install.linux_user_not_added",
                    user = user,
                    group = group,
                    error = e
                )
                .into_owned();
            }
        }
    }

    std::fs::write(&unit_path, unit)
        .map_err(|e| format!("cannot write {unit_path} (run as root?): {e}"))?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", crate::LINUX_UNIT_NAME])?;

    println!(
        "{}{group_note}",
        tr!(
            "install.linux_installed",
            path = unit_path,
            unit = crate::LINUX_UNIT_NAME
        )
    );
    Ok(())
}

/// Create the access group if it does not exist (idempotent). A system group
/// (no login, low GID). Missing `groupadd` is a hard error — without the group
/// only root could reach the service.
#[cfg(not(target_os = "windows"))]
fn ensure_group(group: &str) -> Result<(), Box<dyn std::error::Error>> {
    // `getent group <g>` exits 0 if it exists; skip creation then.
    let exists = std::process::Command::new("getent")
        .args(["group", group])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if exists {
        return Ok(());
    }
    let status = std::process::Command::new("groupadd")
        .args(["--system", group])
        .status()
        .map_err(|e| format!("cannot run groupadd (run as root?): {e}"))?;
    if !status.success() {
        return Err(format!("`groupadd --system {group}` failed ({status})").into());
    }
    Ok(())
}

/// The human to grant service access to: the user behind `sudo` if present,
/// else `$USER`/`LOGNAME`. `None` when only a bare-root context is available.
#[cfg(not(target_os = "windows"))]
fn install_target_user() -> Option<String> {
    for var in ["SUDO_USER", "USER", "LOGNAME"] {
        if let Ok(v) = std::env::var(var) {
            let v = v.trim();
            if !v.is_empty() && v != "root" {
                return Some(v.to_string());
            }
        }
    }
    None
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

#[cfg(not(target_os = "windows"))]
pub fn uninstall() -> Result<(), Box<dyn std::error::Error>> {
    // Best-effort: disable+stop, then remove the unit file and reload.
    let _ = systemctl(&["disable", "--now", crate::LINUX_UNIT_NAME]);
    let unit_path = format!("/etc/systemd/system/{}.service", crate::LINUX_UNIT_NAME);
    if let Err(e) = std::fs::remove_file(&unit_path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            return Err(format!("cannot remove {unit_path} (run as root?): {e}").into());
        }
    }
    let _ = systemctl(&["daemon-reload"]);
    // Best-effort: drop the access group we created (ignore if in use / absent).
    let _ = std::process::Command::new("groupdel")
        .arg(own_backup_common::UNIX_ACCESS_GROUP)
        .status();
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

    if !dir.exists() {
        println!("{}", tr!("install.purge_nothing", path = dir.display()));
        return Ok(());
    }

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

#[cfg(not(target_os = "windows"))]
fn installed_binary() -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    // systemd's unit names it; the installer writes the unit, so this is the
    // same path it used.
    Ok(std::path::PathBuf::from(
        "/usr/local/bin/own-backup-service",
    ))
}
