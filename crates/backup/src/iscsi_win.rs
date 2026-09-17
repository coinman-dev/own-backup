//! The other half of the mount: Windows' own iSCSI initiator.
//!
//! [`crate::iscsi`] serves the image; this makes the initiator log in to it, so
//! a `\\.\PhysicalDriveN` appears and Windows mounts the filesystem inside.
//! Between them they replace Arsenal Image Mounter completely — no driver of
//! ours is installed, and the one that does the work ships with Windows.
//!
//! Three things have to go right, and each fails in its own way:
//!
//! * **The service.** `MSiSCSI` is set to start on demand and is usually not
//!   running. Group policy can leave it *disabled*, which no amount of asking
//!   will undo — that case is reported with the one command that fixes it.
//! * **The portal.** The initiator is told our address, then asked to log in.
//!   A login that names the portal explicitly is the direct route; when it is
//!   refused we fall back to a static target and to the initiator's own list,
//!   because both have worked where the other did not.
//! * **The disk.** Logging in succeeds before the disk exists: the session is
//!   up, PnP is still building the device. So the device list is polled, and a
//!   timeout says the session is alive but the disk never came — which is a
//!   different problem from a failed login and gets a different message.

#![cfg(target_os = "windows")]

use std::time::{Duration, Instant};

use own_backup_common::{OwnBackupError, Result};
use tracing::{info, warn};
use windows::core::PCWSTR;
use windows::Win32::Storage::IscsiDisc::{
    AddIScsiSendTargetPortalW, AddIScsiStaticTargetW, GetDevicesForIScsiSessionW,
    LoginIScsiTargetW, LogoutIScsiTarget, RemoveIScsiSendTargetPortalW, RemoveIScsiStaticTargetW,
    ISCSI_DEVICE_ON_SESSIONW, ISCSI_TARGET_PORTALW, ISCSI_UNIQUE_SESSION_ID,
};

use crate::win_device::to_wide;

/// The Microsoft iSCSI Initiator service.
pub const INITIATOR_SERVICE: &str = "MSiSCSI";

/// How long the service is given to reach "running".
const SERVICE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long PnP is given to produce the disk after a successful login.
const DISK_TIMEOUT: Duration = Duration::from_secs(30);

fn dev(msg: impl Into<String>) -> OwnBackupError {
    OwnBackupError::Device(msg.into())
}

/// What the initiator's own error numbers mean.
///
/// The `0xEFFF….` range is iSCSI's (`iscsierr.h`); small numbers are ordinary
/// Win32 errors, and telling them apart matters — "the service is disabled" and
/// "the target refused the login" are answered in completely different places.
fn describe(code: u32) -> String {
    let known = match code {
        0xEFFF_0001 => "non-specific error",
        0xEFFF_0002 => "login failed",
        0xEFFF_0003 => "connection failed — the initiator never reached the portal",
        0xEFFF_0004 => "initiator error",
        0xEFFF_0005 => "authentication failure",
        0xEFFF_0006 => "target not found",
        0x0000_0002 => "Win32: not found",
        0x0000_0005 => "Win32: access denied",
        0x0000_0057 => "Win32: invalid parameter",
        0x0000_0422 => "Win32: the service is disabled",
        _ if code < 0x0001_0000 => "a Win32 error, not an iSCSI one",
        _ => "see the initiator's own error list (iscsierr.h)",
    };
    format!("{code:#010x} — {known}")
}

fn check(api: &str, target: &str, code: u32) -> Result<()> {
    if code == 0 {
        return Ok(());
    }
    Err(dev(format!("iSCSI: {api} failed with {}", describe(code))).tap(target))
}

/// Small helper so the target name rides along in the message without repeating
/// the format string at every call site.
trait Tap {
    fn tap(self, target: &str) -> Self;
}
impl Tap for OwnBackupError {
    fn tap(self, target: &str) -> Self {
        match self {
            OwnBackupError::Device(m) if !target.is_empty() => {
                OwnBackupError::Device(format!("{m} (цель {target})"))
            }
            other => other,
        }
    }
}

// ---------------------------------------------------------------------------
// the service
// ---------------------------------------------------------------------------

/// Make sure `MSiSCSI` is running, enabling it first if policy left it
/// disabled. Idempotent: a service already running costs one query.
pub fn ensure_initiator_service() -> Result<()> {
    use windows::Win32::System::Services::{
        ChangeServiceConfigW, CloseServiceHandle, OpenSCManagerW, OpenServiceW,
        QueryServiceConfigW, QueryServiceStatus, StartServiceW, QUERY_SERVICE_CONFIGW,
        SC_MANAGER_CONNECT, SERVICE_CHANGE_CONFIG, SERVICE_DEMAND_START, SERVICE_DISABLED,
        SERVICE_NO_CHANGE, SERVICE_QUERY_CONFIG, SERVICE_QUERY_STATUS, SERVICE_RUNNING,
        SERVICE_START, SERVICE_STATUS,
    };

    let name = to_wide(INITIATOR_SERVICE);
    // SAFETY: a null machine/database means the local one.
    let scm = unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) }
        .map_err(|e| dev(format!("iSCSI: cannot open the SCM: {e}")))?;

    let result = (|| -> Result<()> {
        // SAFETY: `name` is NUL-terminated and outlives the call.
        let service = unsafe {
            OpenServiceW(
                scm,
                PCWSTR(name.as_ptr()),
                SERVICE_QUERY_STATUS | SERVICE_START | SERVICE_QUERY_CONFIG | SERVICE_CHANGE_CONFIG,
            )
        }
        .map_err(|e| {
            dev(format!(
                "iSCSI: the Microsoft iSCSI Initiator service ({INITIATOR_SERVICE}) is not \
                 present: {e}. In WinPE it needs its files and the Event Log workaround."
            ))
        })?;

        let done = (|| -> Result<()> {
            let mut status = SERVICE_STATUS::default();
            // SAFETY: live handle and out-parameter.
            if unsafe { QueryServiceStatus(service, &mut status) }.is_ok()
                && status.dwCurrentState == SERVICE_RUNNING
            {
                return Ok(());
            }

            // Disabled is not something StartService can talk its way out of.
            let mut needed = 0u32;
            let mut buf = vec![0u8; 8192];
            // SAFETY: the buffer and its length travel together.
            let cfg_ok = unsafe {
                QueryServiceConfigW(
                    service,
                    Some(buf.as_mut_ptr() as *mut QUERY_SERVICE_CONFIGW),
                    buf.len() as u32,
                    &mut needed,
                )
            }
            .is_ok();
            if cfg_ok {
                // SAFETY: the call above filled the buffer with this struct.
                let cfg = unsafe { &*(buf.as_ptr() as *const QUERY_SERVICE_CONFIGW) };
                if cfg.dwStartType == SERVICE_DISABLED {
                    warn!("the iSCSI initiator service is DISABLED — switching it to demand start");
                    // SAFETY: only the start type is changed; every other field
                    // is SERVICE_NO_CHANGE or null.
                    unsafe {
                        ChangeServiceConfigW(
                            service,
                            windows::Win32::System::Services::ENUM_SERVICE_TYPE(SERVICE_NO_CHANGE),
                            SERVICE_DEMAND_START,
                            windows::Win32::System::Services::SERVICE_ERROR(SERVICE_NO_CHANGE),
                            PCWSTR::null(),
                            PCWSTR::null(),
                            None,
                            PCWSTR::null(),
                            PCWSTR::null(),
                            PCWSTR::null(),
                            PCWSTR::null(),
                        )
                    }
                    .map_err(|e| {
                        dev(format!(
                            "iSCSI: the Microsoft iSCSI Initiator service ({INITIATOR_SERVICE}) \
                             is disabled and could not be enabled: {e}. Run this once as an \
                             administrator: `sc config {INITIATOR_SERVICE} start= demand` \
                             (a group policy may be holding it disabled)."
                        ))
                    })?;
                    info!("start type changed to demand start");
                }
            }

            // SAFETY: live handle, no arguments to the service.
            if let Err(e) = unsafe { StartServiceW(service, None) } {
                // Already running is not a failure; anything else is.
                let mut status = SERVICE_STATUS::default();
                // SAFETY: as above.
                let running = unsafe { QueryServiceStatus(service, &mut status) }.is_ok()
                    && status.dwCurrentState == SERVICE_RUNNING;
                if !running {
                    return Err(dev(format!(
                        "iSCSI: the Microsoft iSCSI Initiator service ({INITIATOR_SERVICE}) \
                         would not start: {e}"
                    )));
                }
            }

            let deadline = Instant::now() + SERVICE_TIMEOUT;
            loop {
                let mut status = SERVICE_STATUS::default();
                // SAFETY: as above.
                if unsafe { QueryServiceStatus(service, &mut status) }.is_ok()
                    && status.dwCurrentState == SERVICE_RUNNING
                {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err(dev(format!(
                        "iSCSI: the Microsoft iSCSI Initiator service ({INITIATOR_SERVICE}) did \
                         not reach the running state within {} s",
                        SERVICE_TIMEOUT.as_secs()
                    )));
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        })();

        // SAFETY: handle from OpenServiceW, closed once.
        let _ = unsafe { CloseServiceHandle(service) };
        done
    })();

    // SAFETY: handle from OpenSCManagerW, closed once.
    let _ = unsafe { CloseServiceHandle(scm) };
    if result.is_ok() {
        info!(service = INITIATOR_SERVICE, "initiator service started");
    }
    result
}

/// Whether this machine can serve an image to itself right now, and what is in
/// the way if it cannot.
///
/// Everything here is a plain fact, gathered without changing anything: the
/// interface shows it, the CLI prints it, and on the rescue medium it is the
/// first thing worth asking when a mount fails.
#[derive(Debug, Clone, serde::Serialize)]
pub struct InitiatorStatus {
    /// Mounting an image will work.
    pub ready: bool,
    /// `msiscsi.sys` — the initiator's kernel half.
    pub driver_file: bool,
    /// The service exists at all (it does not, in a trimmed WinPE image).
    pub service_present: bool,
    pub running: bool,
    /// Policy has it disabled: starting it needs an administrator once.
    pub disabled: bool,
    /// This is the boot medium, where the service needs the Event Log
    /// workaround before it will start.
    pub media_mode: bool,
    pub message: String,
}

pub fn status() -> InitiatorStatus {
    use windows::Win32::System::Services::{
        CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceConfigW, QueryServiceStatus,
        QUERY_SERVICE_CONFIGW, SC_MANAGER_CONNECT, SERVICE_DISABLED, SERVICE_QUERY_CONFIG,
        SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_STATUS,
    };

    let system32 = std::path::PathBuf::from(
        std::env::var("SystemRoot").unwrap_or_else(|_| r"X:\Windows".into()),
    )
    .join("System32");
    let driver_file = system32.join("drivers").join("msiscsi.sys").exists();
    let media_mode = own_backup_common::media_mode::is_media_mode();

    let (mut service_present, mut running, mut disabled) = (false, false, false);
    let name = to_wide(INITIATOR_SERVICE);
    // SAFETY: a null machine/database means the local one.
    if let Ok(scm) = unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) } {
        // SAFETY: `name` outlives the call.
        if let Ok(service) = unsafe {
            OpenServiceW(
                scm,
                PCWSTR(name.as_ptr()),
                SERVICE_QUERY_STATUS | SERVICE_QUERY_CONFIG,
            )
        } {
            service_present = true;
            let mut st = SERVICE_STATUS::default();
            // SAFETY: live handle and out-parameter.
            if unsafe { QueryServiceStatus(service, &mut st) }.is_ok() {
                running = st.dwCurrentState == SERVICE_RUNNING;
            }
            let mut needed = 0u32;
            let mut buf = vec![0u8; 8192];
            // SAFETY: the buffer and its length travel together.
            if unsafe {
                QueryServiceConfigW(
                    service,
                    Some(buf.as_mut_ptr() as *mut QUERY_SERVICE_CONFIGW),
                    buf.len() as u32,
                    &mut needed,
                )
            }
            .is_ok()
            {
                // SAFETY: the call above filled the buffer with this struct.
                let cfg = unsafe { &*(buf.as_ptr() as *const QUERY_SERVICE_CONFIGW) };
                disabled = cfg.dwStartType == SERVICE_DISABLED;
            }
            // SAFETY: handle from OpenServiceW, closed once.
            let _ = unsafe { CloseServiceHandle(service) };
        }
        // SAFETY: handle from OpenSCManagerW, closed once.
        let _ = unsafe { CloseServiceHandle(scm) };
    }

    // Ready means "a mount would work now": the service may be stopped, we
    // start it on demand — but it has to exist and be allowed to start.
    let ready = driver_file && service_present && !disabled;
    let message = if !driver_file {
        format!(
            "Драйвер инициатора (msiscsi.sys) не найден в {}. Подключать образы нечем.",
            system32.join("drivers").display()
        )
    } else if !service_present {
        let extra = if media_mode {
            " На носителе службе нужен журнал событий: его включает start-media.cmd."
        } else {
            ""
        };
        format!("Службы {INITIATOR_SERVICE} нет в системе.{extra}")
    } else if disabled {
        format!(
            "Служба {INITIATOR_SERVICE} отключена политикой. Включить один раз от имени \
             администратора: sc config {INITIATOR_SERVICE} start= demand"
        )
    } else if running {
        format!("Всё на месте: служба {INITIATOR_SERVICE} работает.")
    } else {
        format!("Всё на месте: служба {INITIATOR_SERVICE} запустится по требованию.")
    };

    InitiatorStatus { ready, driver_file, service_present, running, disabled, media_mode, message }
}

// ---------------------------------------------------------------------------
// login / logout
// ---------------------------------------------------------------------------

/// A live session and the disk it produced. Logging out is [`Self::logout`];
/// dropping it without that leaves the disk attached, so the caller is expected
/// to call it — which [`crate::attach_win`] does even when its own work failed.
pub struct AttachedTarget {
    session: ISCSI_UNIQUE_SESSION_ID,
    target: String,
    portal: ISCSI_TARGET_PORTALW,
    portal_added: bool,
    static_target_added: bool,
    pub disk_number: u32,
}

fn portal_for(address: &str, port: u16) -> ISCSI_TARGET_PORTALW {
    let mut portal = ISCSI_TARGET_PORTALW { Socket: port, ..Default::default() };
    for (i, c) in address.encode_utf16().take(portal.Address.len() - 1).enumerate() {
        portal.Address[i] = c;
    }
    portal
}

/// Log the initiator in to our target and wait for its disk.
pub fn login(address: &str, port: u16, target: &str) -> Result<AttachedTarget> {
    ensure_initiator_service()?;

    let iqn = to_wide(target);
    let mut portal = portal_for(address, port);

    // The discovery portal: the initiator asks our target what it serves. This
    // is also the proof that it can reach us at all — everything after it
    // assumes the address works.
    let mut portal_added = false;
    // SAFETY: the portal outlives the call; no login options, no security.
    let code = unsafe {
        AddIScsiSendTargetPortalW(PCWSTR::null(), 0, std::ptr::null_mut(), 0, &mut portal)
    };
    if code == 0 {
        portal_added = true;
        info!(%address, port, "iSCSI: discovery portal added — the initiator reached our target");
    } else {
        warn!(
            reason = %describe(code),
            "iSCSI: SendTargets portal not added; falling back to a static target"
        );
        // SAFETY: `iqn` outlives the call; no alias, mappings or portal group.
        let st = unsafe {
            AddIScsiStaticTargetW(
                PCWSTR(iqn.as_ptr()),
                PCWSTR::null(),
                0,
                false,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        check("AddIScsiStaticTarget", target, st)?;
    }

    let mut session = ISCSI_UNIQUE_SESSION_ID::default();
    let mut connection = ISCSI_UNIQUE_SESSION_ID::default();
    // Not informational, not persistent: the session lasts as long as the work
    // does, and nothing of ours survives a reboot in the initiator's database.
    // SAFETY: every pointer either outlives the call or is null by design.
    let mut code = unsafe {
        LoginIScsiTargetW(
            PCWSTR(iqn.as_ptr()),
            false,
            PCWSTR::null(),
            0,
            &mut portal,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            None,
            false,
            &mut session,
            &mut connection,
        )
    };
    if code != 0 {
        warn!(
            reason = %describe(code),
            "iSCSI: login with an explicit portal failed — retrying with the initiator's own list"
        );
        // SAFETY: as above, with no portal: the initiator uses what discovery
        // put in its list.
        code = unsafe {
            LoginIScsiTargetW(
                PCWSTR(iqn.as_ptr()),
                false,
                PCWSTR::null(),
                0,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                None,
                false,
                &mut session,
                &mut connection,
            )
        };
    }
    let mut attached = AttachedTarget {
        session,
        target: target.to_string(),
        portal,
        portal_added,
        static_target_added: !portal_added,
        disk_number: u32::MAX,
    };
    if code != 0 {
        // The session never came up; take back the portal we added.
        attached.cleanup_registrations();
        return Err(dev(format!("iSCSI: LoginIScsiTarget failed with {}", describe(code)))
            .tap(target));
    }

    match wait_for_disk(&mut attached.session) {
        Ok(disk) => {
            attached.disk_number = disk;
            info!(target, %address, port, disk, "iSCSI target attached");
            Ok(attached)
        }
        Err(e) => {
            attached.logout();
            Err(e)
        }
    }
}

/// The disk number of the first device on this session, once PnP has made one.
fn wait_for_disk(session: &mut ISCSI_UNIQUE_SESSION_ID) -> Result<u32> {
    let deadline = Instant::now() + DISK_TIMEOUT;
    // The last code the initiator gave us, for the timeout message: "no disk
    // yet" and "the session is gone" arrive here as different numbers.
    let mut last;
    loop {
        let mut count = 0u32;
        // Ask for the count first: the device list is a few kilobytes per entry
        // and there is no point guessing.
        // SAFETY: a null device array with a zero count is how the size is asked for.
        last = unsafe { GetDevicesForIScsiSessionW(session, &mut count, std::ptr::null_mut()) };
        if count > 0 {
            let mut devices = vec![ISCSI_DEVICE_ON_SESSIONW::default(); count as usize];
            // SAFETY: the array matches the count passed alongside it.
            last = unsafe {
                GetDevicesForIScsiSessionW(session, &mut count, devices.as_mut_ptr())
            };
            if last == 0 {
                if let Some(disk) = devices
                    .iter()
                    .map(|d| d.StorageDeviceNumber.DeviceNumber)
                    .find(|n| *n != u32::MAX)
                {
                    return Ok(disk);
                }
            }
        }
        if Instant::now() >= deadline {
            return Err(dev(format!(
                "iSCSI: logged in, but no disk appeared within {} s (last code {}). The session \
                 exists — check Disk Management and the initiator's session list.",
                DISK_TIMEOUT.as_secs(),
                describe(last)
            )));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

impl AttachedTarget {
    /// Log the session out and take back what was registered for it.
    ///
    /// Best effort by design: this runs on the failure path too, and a portal
    /// that would not go away must not hide the error that got us here.
    pub fn logout(mut self) {
        // SAFETY: the session id came from a successful login.
        let code = unsafe { LogoutIScsiTarget(&mut self.session) };
        if code == 0 {
            info!(target = %self.target, disk = self.disk_number, "iSCSI session logged out");
        } else {
            warn!(target = %self.target, reason = %describe(code), "iSCSI: logout refused");
        }
        self.cleanup_registrations();
    }

    fn cleanup_registrations(&mut self) {
        if self.static_target_added {
            let iqn = to_wide(&self.target);
            // SAFETY: `iqn` outlives the call.
            let code = unsafe { RemoveIScsiStaticTargetW(PCWSTR(iqn.as_ptr())) };
            if code != 0 {
                warn!(reason = %describe(code), "iSCSI: static target not removed");
            }
            self.static_target_added = false;
        }
        if self.portal_added {
            // SAFETY: the portal struct outlives the call.
            let code = unsafe {
                RemoveIScsiSendTargetPortalW(PCWSTR::null(), 0, &mut self.portal)
            };
            if code != 0 {
                warn!(reason = %describe(code), "iSCSI: discovery portal not removed");
            }
            self.portal_added = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three codes worth telling apart by name: everything else is handed
    /// over with its number so it can be looked up rather than guessed at.
    #[test]
    fn the_initiators_own_error_numbers_are_translated() {
        assert!(describe(0xEFFF_0003).contains("never reached the portal"));
        assert!(describe(0x0000_0005).contains("access denied"));
        assert!(describe(0x0000_0422).contains("service is disabled"));
        let unknown = describe(0xEFFF_0099);
        assert!(unknown.contains("0xefff0099"), "{unknown}");
        assert!(unknown.contains("iscsierr.h"), "{unknown}");
    }

    #[test]
    fn a_portal_carries_the_address_it_was_given() {
        let p = portal_for("127.0.0.1", 3260);
        assert_eq!(p.Socket, 3260);
        let text: String =
            String::from_utf16_lossy(&p.Address).trim_end_matches('\0').to_string();
        assert_eq!(text, "127.0.0.1");
    }
}
