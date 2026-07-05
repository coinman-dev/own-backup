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
//! Registration needs Administrator (Windows) / root (Linux); the OS refuses
//! otherwise with a clear access-denied error that we surface verbatim.

// ---------------------------------------------------------------------------
// Windows: Service Control Manager
// ---------------------------------------------------------------------------

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

    let service =
        manager.create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START)?;
    service.set_description(crate::SERVICE_DESCRIPTION)?;

    println!(
        "Служба «{}» ({}) установлена.\n  \
         Путь: {}\n  \
         Автозапуск при загрузке, учётная запись LocalSystem.\n  \
         Запустить сейчас:  sc start {}",
        crate::SERVICE_DISPLAY_NAME,
        crate::SERVICE_NAME,
        exe.display(),
        crate::SERVICE_NAME,
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
        "Служба «{}» ({}) удалена.",
        crate::SERVICE_DISPLAY_NAME,
        crate::SERVICE_NAME
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
        println!("Служба «{}» уже запущена.", crate::SERVICE_DISPLAY_NAME);
        return Ok(());
    }
    let no_args: [&std::ffi::OsStr; 0] = [];
    service.start(&no_args)?;
    println!("Служба «{}» запущена.", crate::SERVICE_DISPLAY_NAME);
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn stop() -> Result<(), Box<dyn std::error::Error>> {
    use std::time::Duration;
    use windows_service::service::{ServiceAccess, ServiceState};
    let service = open_service(ServiceAccess::QUERY_STATUS | ServiceAccess::STOP)?;
    if service.query_status()?.current_state == ServiceState::Stopped {
        println!("Служба «{}» уже остановлена.", crate::SERVICE_DISPLAY_NAME);
        return Ok(());
    }
    service.stop()?;
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(300));
        if let Ok(s) = service.query_status() {
            if s.current_state == ServiceState::Stopped {
                break;
            }
        }
    }
    println!("Служба «{}» остановлена.", crate::SERVICE_DISPLAY_NAME);
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn status() -> Result<(), Box<dyn std::error::Error>> {
    use windows_service::service::ServiceAccess;
    let service = open_service(ServiceAccess::QUERY_STATUS)?;
    let s = service.query_status()?;
    println!(
        "Служба «{}» ({}): {:?}",
        crate::SERVICE_DISPLAY_NAME,
        crate::SERVICE_NAME,
        s.current_state
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

    std::fs::write(&unit_path, unit)
        .map_err(|e| format!("cannot write {unit_path} (run as root?): {e}"))?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", crate::LINUX_UNIT_NAME])?;

    println!(
        "systemd-юнит установлен и запущен: {unit_path}\n  \
         Статус:  systemctl status {}.service",
        crate::LINUX_UNIT_NAME
    );
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
    println!("systemd-юнит {}.service удалён.", crate::LINUX_UNIT_NAME);
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub fn start() -> Result<(), Box<dyn std::error::Error>> {
    systemctl(&["start", crate::LINUX_UNIT_NAME])?;
    println!("systemd-юнит {}.service запущен.", crate::LINUX_UNIT_NAME);
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub fn stop() -> Result<(), Box<dyn std::error::Error>> {
    systemctl(&["stop", crate::LINUX_UNIT_NAME])?;
    println!("systemd-юнит {}.service остановлен.", crate::LINUX_UNIT_NAME);
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
    let status = std::process::Command::new("systemctl").args(args).status()?;
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
