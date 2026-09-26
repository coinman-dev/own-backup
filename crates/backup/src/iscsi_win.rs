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

use std::net::{IpAddr, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use own_backup_common::{tr, OwnBackupError, Result};
use tracing::{info, warn};
use windows::core::PCWSTR;
use windows::Win32::Storage::IscsiDisc::{
    AddIScsiSendTargetPortalW, AddIScsiStaticTargetW, GetDevicesForIScsiSessionW,
    GetIScsiSessionListW, LoginIScsiTargetW, LogoutIScsiTarget, RemoveIScsiSendTargetPortalW,
    RemoveIScsiStaticTargetW, ISCSI_CONNECTION_INFOW, ISCSI_DEVICE_ON_SESSIONW,
    ISCSI_SESSION_INFOW, ISCSI_TARGET_PORTALW, ISCSI_TARGET_PORTAL_GROUPW,
    ISCSI_UNIQUE_SESSION_ID,
};

use crate::win_device::to_wide;

/// The Microsoft iSCSI Initiator service.
pub const INITIATOR_SERVICE: &str = "MSiSCSI";

/// "any initiator port" — `ISCSI_ANY_INITIATOR_PORT` from `iscsidsc.h`.
///
/// NOT zero, which is what this passed at first: zero names initiator port 0,
/// which does not exist, so `AddIScsiSendTargetPortal` answered
/// ERROR_INVALID_PARAMETER, the portal was never registered, and the login
/// that followed had nowhere to go — `ISDSC_NO_PORTAL_SPECIFIED`
/// (0xefff003c, seen 2026-09-18).
const ANY_INITIATOR_PORT: u32 = u32::MAX;

/// How long the service is given to reach "running".
const SERVICE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long PnP is given to produce the disk after a successful login.
const DISK_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a logout waits out "a device on that session is in use".
const LOGOUT_PATIENCE: Duration = Duration::from_secs(20);
/// How long a session's target is given to answer before it counts as gone.
const TARGET_ANSWER_TIMEOUT: Duration = Duration::from_millis(300);

const ISDSC_INVALID_SESSION_ID: u32 = 0xefff_001c;
const ISDSC_SESSION_BUSY: u32 = 0xefff_001f;
const ISDSC_DEVICE_BUSY_ON_SESSION: u32 = 0xefff_0040;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;

fn dev(msg: impl Into<String>) -> OwnBackupError {
    OwnBackupError::Device(msg.into())
}

/// Every status the initiator can return, taken from `iscsierr.h` in the
/// Windows SDK: the full code, its name and the text Microsoft ships with
/// it. Keyed on the whole value because the informational and error sets
/// reuse the same low half. Without this a log line is a bare number and
/// the next person goes looking for a header they may not have — which is
/// exactly what `0xefff003c` cost on 2026-09-18.
const ISDSC_STATUSES: [(u32, &str, &str); 104] = [
    (0xafff002e, "ISDSC_OPERATION_NOT_ALL_SUCCESS", "The operation was not successful for all initiators or discovery methods"),
    (0xafff003b, "ISDSC_OPERATION_REQUIRES_REBOOT", "The operation was successful but requires a driver reload or reboot to become effective"),
    (0xafff0048, "ISDSC_SHORT_CHAP_SECRET", "The specified CHAP secret is less than 96 bits and will not be usable for authenticating over non ipsec connections"),
    (0xefff0001, "ISDSC_NON_SPECIFIC_ERROR", "A non specific error occurred"),
    (0xefff0002, "ISDSC_LOGIN_FAILED", "Login Failed"),
    (0xefff0003, "ISDSC_CONNECTION_FAILED", "Connection Failed"),
    (0xefff0004, "ISDSC_INITIATOR_NODE_ALREADY_EXISTS", "Initiator Node Already Exists"),
    (0xefff0005, "ISDSC_INITIATOR_NODE_NOT_FOUND", "Initiator Node Does Not Exist"),
    (0xefff0006, "ISDSC_TARGET_MOVED_TEMPORARILY", "Target Moved Temporarily"),
    (0xefff0007, "ISDSC_TARGET_MOVED_PERMANENTLY", "Target Moved Permanently"),
    (0xefff0008, "ISDSC_INITIATOR_ERROR", "Initiator Error"),
    (0xefff0009, "ISDSC_AUTHENTICATION_FAILURE", "Authentication Failure"),
    (0xefff000a, "ISDSC_AUTHORIZATION_FAILURE", "Authorization Failure"),
    (0xefff000b, "ISDSC_NOT_FOUND", "Not Found"),
    (0xefff000c, "ISDSC_TARGET_REMOVED", "Target Removed"),
    (0xefff000d, "ISDSC_UNSUPPORTED_VERSION", "Unsupported Version"),
    (0xefff000e, "ISDSC_TOO_MANY_CONNECTIONS", "Too many Connections"),
    (0xefff000f, "ISDSC_MISSING_PARAMETER", "Missing Parameter"),
    (0xefff0010, "ISDSC_CANT_INCLUDE_IN_SESSION", "Can not include in session"),
    (0xefff0011, "ISDSC_SESSION_TYPE_NOT_SUPPORTED", "Session type not supported"),
    (0xefff0012, "ISDSC_TARGET_ERROR", "Target Error"),
    (0xefff0013, "ISDSC_SERVICE_UNAVAILABLE", "Service Unavailable"),
    (0xefff0014, "ISDSC_OUT_OF_RESOURCES", "Out of Resources"),
    (0xefff0015, "ISDSC_CONNECTION_ALREADY_EXISTS", "Connections already exist on initiator node"),
    (0xefff0016, "ISDSC_SESSION_ALREADY_EXISTS", "Session Already Exists"),
    (0xefff0017, "ISDSC_INITIATOR_INSTANCE_NOT_FOUND", "Initiator Instance Does Not Exist"),
    (0xefff0018, "ISDSC_TARGET_ALREADY_EXISTS", "Target Already Exists"),
    (0xefff0019, "ISDSC_DRIVER_BUG", "The iscsi driver implementation did not complete an operation correctly"),
    (0xefff001a, "ISDSC_INVALID_TEXT_KEY", "An invalid key text was encountered"),
    (0xefff001b, "ISDSC_INVALID_SENDTARGETS_TEXT", "Invalid SendTargets response text was encountered"),
    (0xefff001c, "ISDSC_INVALID_SESSION_ID", "Invalid Session Id"),
    (0xefff001d, "ISDSC_SCSI_REQUEST_FAILED", "The scsi request failed"),
    (0xefff001e, "ISDSC_TOO_MANY_SESSIONS", "Exceeded max sessions for this initiator"),
    (0xefff001f, "ISDSC_SESSION_BUSY", "Session is busy since a request is already in progress"),
    (0xefff0020, "ISDSC_TARGET_MAPPING_UNAVAILABLE", "The target mapping requested is not available"),
    (0xefff0021, "ISDSC_ADDRESS_TYPE_NOT_SUPPORTED", "The Target Address type given is not supported"),
    (0xefff0022, "ISDSC_LOGON_FAILED", "Logon Failed"),
    (0xefff0023, "ISDSC_SEND_FAILED", "TCP Send Failed"),
    (0xefff0024, "ISDSC_TRANSPORT_ERROR", "TCP Transport Error"),
    (0xefff0025, "ISDSC_VERSION_MISMATCH", "iSCSI Version Mismatch"),
    (0xefff0026, "ISDSC_TARGET_MAPPING_OUT_OF_RANGE", "The Target Mapping Address passed is out of range for the adapter configuration"),
    (0xefff0027, "ISDSC_TARGET_PRESHAREDKEY_UNAVAILABLE", "The preshared key for the target or IKE identification payload is not available"),
    (0xefff0028, "ISDSC_TARGET_AUTHINFO_UNAVAILABLE", "The authentication information for the target is not available"),
    (0xefff0029, "ISDSC_TARGET_NOT_FOUND", "The target name is not found or is marked as hidden from login"),
    (0xefff002a, "ISDSC_LOGIN_USER_INFO_BAD", "One or more parameters specified in LoginTargetIN structure is invalid"),
    (0xefff002b, "ISDSC_TARGET_MAPPING_EXISTS", "Given target mapping already exists"),
    (0xefff002c, "ISDSC_HBA_SECURITY_CACHE_FULL", "The HBA security information cache is full"),
    (0xefff002d, "ISDSC_INVALID_PORT_NUMBER", "The port number passed is not valid for the initiator"),
    (0xefff002f, "ISDSC_HBA_SECURITY_CACHE_NOT_SUPPORTED", "The HBA security information cache is not supported by this adapter"),
    (0xefff0030, "ISDSC_IKE_ID_PAYLOAD_TYPE_NOT_SUPPORTED", "The IKE id payload type specified is not supported"),
    (0xefff0031, "ISDSC_IKE_ID_PAYLOAD_INCORRECT_SIZE", "The IKE id payload size specified is not correct"),
    (0xefff0032, "ISDSC_TARGET_PORTAL_ALREADY_EXISTS", "Target Portal Structure Already Exists"),
    (0xefff0033, "ISDSC_TARGET_ADDRESS_ALREADY_EXISTS", "Target Address Structure Already Exists"),
    (0xefff0034, "ISDSC_NO_AUTH_INFO_AVAILABLE", "There is no IKE authentication information available"),
    (0xefff0035, "ISDSC_NO_TUNNEL_OUTER_MODE_ADDRESS", "There is no tunnel mode outer address specified"),
    (0xefff0036, "ISDSC_CACHE_CORRUPTED", "Authentication or tunnel address cache is corrupted"),
    (0xefff0037, "ISDSC_REQUEST_NOT_SUPPORTED", "The request or operation is not supported"),
    (0xefff0038, "ISDSC_TARGET_OUT_OF_RESORCES", "The target does not have enough resources to process the given request"),
    (0xefff0039, "ISDSC_SERVICE_DID_NOT_RESPOND", "The initiator service did not respond to the request sent by the driver"),
    (0xefff003a, "ISDSC_ISNS_SERVER_NOT_FOUND", "The Internet Storage Name Server (iSNS) server was not found or is unavailable"),
    (0xefff003c, "ISDSC_NO_PORTAL_SPECIFIED", "There is no target portal available to complete the login"),
    (0xefff003d, "ISDSC_CANT_REMOVE_LAST_CONNECTION", "Cannot remove the last connection for a session"),
    (0xefff003e, "ISDSC_SERVICE_NOT_RUNNING", "The Microsoft iSCSI Initiator Service is not running. Please start the service and retry"),
    (0xefff003f, "ISDSC_TARGET_ALREADY_LOGGED_IN", "The target has already been logged in via an iSCSI session"),
    (0xefff0040, "ISDSC_DEVICE_BUSY_ON_SESSION", "The session cannot be logged out since a device on that session is currently being used"),
    (0xefff0041, "ISDSC_COULD_NOT_SAVE_PERSISTENT_LOGIN_DATA", "Failed to save persistent login information"),
    (0xefff0042, "ISDSC_COULD_NOT_REMOVE_PERSISTENT_LOGIN_DATA", "Failed to remove persistent login information"),
    (0xefff0043, "ISDSC_PORTAL_NOT_FOUND", "The specified portal was not found"),
    (0xefff0044, "ISDSC_INITIATOR_NOT_FOUND", "The specified initiator name was not found"),
    (0xefff0045, "ISDSC_DISCOVERY_MECHANISM_NOT_FOUND", "The specified discovery mechanism was not found"),
    (0xefff0046, "ISDSC_IPSEC_NOT_SUPPORTED_ON_OS", "iSCSI does not support IPSEC for this version of the OS"),
    (0xefff0047, "ISDSC_PERSISTENT_LOGIN_TIMEOUT", "The iSCSI service timed out waiting for all persistent logins to complete"),
    (0xefff0049, "ISDSC_EVALUATION_PEROID_EXPIRED", "The evaluation period for the iSCSI initiator service has expired"),
    (0xefff004a, "ISDSC_INVALID_CHAP_SECRET", "CHAP secret given does not conform to the standard. Please see system event log for more information"),
    (0xefff004b, "ISDSC_INVALID_TARGET_CHAP_SECRET", "Target CHAP secret given is invalid. Maximum size of CHAP secret is 16 bytes. Minimum size is 12 bytes if IPSec is not used"),
    (0xefff004c, "ISDSC_INVALID_INITIATOR_CHAP_SECRET", "Initiator CHAP secret given is invalid. Maximum size of CHAP secret is 16 bytes. Minimum size is 12 bytes if IPSec is not used"),
    (0xefff004d, "ISDSC_INVALID_CHAP_USER_NAME", "CHAP Username given is invalid"),
    (0xefff004e, "ISDSC_INVALID_LOGON_AUTH_TYPE", "Logon Authentication type given is invalid"),
    (0xefff004f, "ISDSC_INVALID_TARGET_MAPPING", "Target Mapping information given is invalid"),
    (0xefff0050, "ISDSC_INVALID_TARGET_ID", "Target Id given in Target Mapping is invalid"),
    (0xefff0051, "ISDSC_INVALID_ISCSI_NAME", "The iSCSI name specified contains invalid characters or is too long"),
    (0xefff0052, "ISDSC_INCOMPATIBLE_ISNS_VERSION", "The version number returned from the Internet Storage Name Server (iSNS) server is not compatible with this version of the iSNS client"),
    (0xefff0053, "ISDSC_FAILED_TO_CONFIGURE_IPSEC", "Initiator failed to configure IPSec for the given connection. This could be because of low resources"),
    (0xefff0054, "ISDSC_BUFFER_TOO_SMALL", "The buffer given for processing the request is too small"),
    (0xefff0055, "ISDSC_INVALID_LOAD_BALANCE_POLICY", "The given Load Balance policy is not recognized by iScsi initiator"),
    (0xefff0056, "ISDSC_INVALID_PARAMETER", "One or more paramaters specified is not valid"),
    (0xefff0057, "ISDSC_DUPLICATE_PATH_SPECIFIED", "Duplicate PathIds were specified in the call to set Load Balance Policy"),
    (0xefff0058, "ISDSC_PATH_COUNT_MISMATCH", "Number of paths specified in Set Load Balance Policy does not match the number of paths to the target"),
    (0xefff0059, "ISDSC_INVALID_PATH_ID", "Path Id specified in the call to set Load Balance Policy is not valid"),
    (0xefff005a, "ISDSC_MULTIPLE_PRIMARY_PATHS_SPECIFIED", "Multiple primary paths specified when only one primary path is expected"),
    (0xefff005b, "ISDSC_NO_PRIMARY_PATH_SPECIFIED", "No primary path specified when at least one is expected"),
    (0xefff005c, "ISDSC_DEVICE_ALREADY_PERSISTENTLY_BOUND", "Device is already a persistently bound device"),
    (0xefff005d, "ISDSC_DEVICE_NOT_FOUND", "Device was not found"),
    (0xefff005e, "ISDSC_DEVICE_NOT_ISCSI_OR_PERSISTENT", "The device specified does not originate from an iSCSI disk or a persistent iSCSI login"),
    (0xefff005f, "ISDSC_DNS_NAME_UNRESOLVED", "The DNS name specified was not resolved"),
    (0xefff0060, "ISDSC_NO_CONNECTION_AVAILABLE", "There is no connection available in the iSCSI session to process the request"),
    (0xefff0061, "ISDSC_LB_POLICY_NOT_SUPPORTED", "The given Load Balance policy is not supported"),
    (0xefff0062, "ISDSC_REMOVE_CONNECTION_IN_PROGRESS", "A remove connection request is already in progress for this session"),
    (0xefff0063, "ISDSC_INVALID_CONNECTION_ID", "Given connection was not found in the session"),
    (0xefff0064, "ISDSC_CANNOT_REMOVE_LEADING_CONNECTION", "The leading connection in the session cannot be removed"),
    (0xefff0065, "ISDSC_RESTRICTED_BY_GROUP_POLICY", "The operation cannot be performed since it does not conform with the group policy assigned to this computer"),
    (0xefff0066, "ISDSC_ISNS_FIREWALL_BLOCKED", "The operation cannot be performed since the Internet Storage Name Server (iSNS) firewall exception has not been enabled"),
    (0xefff0067, "ISDSC_FAILURE_TO_PERSIST_LB_POLICY", "Failed to persist load balancing policy parameters"),
    (0xefff0068, "ISDSC_INVALID_HOST", "The name could not be resolved to an IP Address"),
];

/// What one of those numbers means, in words.
///
/// The `0xEFFF….` range is iSCSI's own; small numbers are ordinary Win32
/// errors, and telling them apart matters — "the service is disabled" and
/// "the target refused the login" are answered in completely different places.
fn describe(code: u32) -> String {
    if let Some((_, name, text)) = ISDSC_STATUSES.iter().find(|(c, _, _)| *c == code) {
        return format!("{code:#010x} {name} — {text}");
    }
    let win32 = match code {
        0x0000_0002 => tr!("iscsi_win.win32_not_found"),
        0x0000_0005 => tr!("iscsi_win.win32_access_denied"),
        0x0000_0057 => tr!("iscsi_win.win32_invalid_parameter"),
        0x0000_0422 => tr!("iscsi_win.win32_service_disabled"),
        _ if code < 0x0001_0000 => tr!("iscsi_win.win32_not_iscsi"),
        _ => tr!("iscsi_win.not_in_iscsierr"),
    };
    format!("{code:#010x} — {win32}")
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
                OwnBackupError::Device(
                    tr!("iscsi_win.with_target", message = m, target = target).into_owned(),
                )
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
        tr!("iscsi_win.driver_missing", path = system32.join("drivers").display()).into_owned()
    } else if !service_present {
        if media_mode {
            tr!("iscsi_win.service_missing_media", service = INITIATOR_SERVICE).into_owned()
        } else {
            tr!("iscsi_win.service_missing", service = INITIATOR_SERVICE).into_owned()
        }
    } else if disabled {
        tr!("iscsi_win.service_disabled_by_policy", service = INITIATOR_SERVICE).into_owned()
    } else if running {
        tr!("iscsi_win.ready_running", service = INITIATOR_SERVICE).into_owned()
    } else {
        tr!("iscsi_win.ready_on_demand", service = INITIATOR_SERVICE).into_owned()
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
    clear_stale_sessions(target, (address, port))?;

    let iqn = to_wide(target);
    let mut portal = portal_for(address, port);

    // The discovery portal: the initiator asks our target what it serves. This
    // is also the proof that it can reach us at all — everything after it
    // assumes the address works.
    let mut portal_added = false;
    // SAFETY: the portal outlives the call; no login options, no security.
    let code = unsafe {
        AddIScsiSendTargetPortalW(
            PCWSTR::null(),
            ANY_INITIATOR_PORT,
            std::ptr::null_mut(),
            0,
            &mut portal,
        )
    };
    if code == 0 {
        portal_added = true;
        info!(%address, port, "iSCSI: discovery portal added — the initiator reached our target");
    } else {
        warn!(
            reason = %describe(code),
            "iSCSI: SendTargets portal not added; falling back to a static target"
        );
        // WITH a portal group: a static target that names no portal leaves the
        // initiator nothing to connect to, and the login then fails with
        // ISDSC_NO_PORTAL_SPECIFIED — which is how this was found.
        let mut group = ISCSI_TARGET_PORTAL_GROUPW { Count: 1, Portals: [portal] };
        // SAFETY: `iqn` and `group` outlive the call; no alias or mappings.
        let st = unsafe {
            AddIScsiStaticTargetW(
                PCWSTR(iqn.as_ptr()),
                PCWSTR::null(),
                0,
                false,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut group,
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
            ANY_INITIATOR_PORT,
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
                ANY_INITIATOR_PORT,
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
    /// Log the session out and take back what was registered for it. `false`
    /// when the initiator would not let go: the session then outlives this
    /// mount, and the next login to the same image clears it
    /// ([`clear_stale_sessions`]).
    ///
    /// Best effort by design: this runs on the failure path too, and a portal
    /// that would not go away must not hide the error that got us here.
    pub fn logout(mut self) -> bool {
        let done = logout_session(&mut self.session, &self.target);
        if done {
            info!(target = %self.target, disk = self.disk_number, "iSCSI session logged out");
        }
        self.cleanup_registrations();
        done
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
            // ANY_INITIATOR_PORT, not 0 — the same value the portal was ADDED
            // with, and for the same reason: zero names initiator port 0, which
            // does not exist. This call had been left at 0, so every teardown
            // answered ERROR_INVALID_PARAMETER (0x57) and the portal stayed
            // registered, accumulating one per mount for the life of the
            // machine. The mistake was found and fixed on the add side on
            // 2026-09-18; its mirror went unnoticed because a portal that is
            // not removed breaks nothing immediately.
            // SAFETY: the portal struct outlives the call.
            let code = unsafe {
                RemoveIScsiSendTargetPortalW(PCWSTR::null(), ANY_INITIATOR_PORT, &mut self.portal)
            };
            if code != 0 {
                warn!(reason = %describe(code), "iSCSI: discovery portal not removed");
            }
            self.portal_added = false;
        }
    }
}

/// Log a session out, waiting out a device that is still being let go.
///
/// A disk whose volumes have just been dismounted and which has just been set
/// offline is not free yet: PnP takes the volume stack down behind our back,
/// and until it has, the initiator answers `ISDSC_DEVICE_BUSY_ON_SESSION`.
/// That is what happened on 2026-09-23, 44 ms after the teardown of the
/// exclusion pass inside an `.obk`: the logout was tried once, the target was
/// stopped under a session still logged in, and the session outlived three
/// service restarts — the image's next mount, 47 minutes later, was refused
/// with `ISDSC_TARGET_ALREADY_LOGGED_IN`.
fn logout_session(session: &mut ISCSI_UNIQUE_SESSION_ID, target: &str) -> bool {
    let start = Instant::now();
    let mut attempts = 0u64;
    loop {
        attempts += 1;
        // SAFETY: the id came from the initiator (a login or its session list).
        let code = unsafe { LogoutIScsiTarget(session) };
        let waited_ms = start.elapsed().as_millis() as u64;
        match code {
            0 => {
                if attempts > 1 {
                    info!(target, attempts, waited_ms, "iSCSI: logout went through once the device was let go");
                }
                return true;
            }
            // Gone already — which is all a logout is for.
            ISDSC_INVALID_SESSION_ID => return true,
            ISDSC_DEVICE_BUSY_ON_SESSION | ISDSC_SESSION_BUSY if start.elapsed() < LOGOUT_PATIENCE => {
                std::thread::sleep(Duration::from_millis((100 * attempts).min(1000)));
            }
            _ => {
                warn!(target, attempts, waited_ms, reason = %describe(code), "iSCSI: logout refused");
                return false;
            }
        }
    }
}

/// One session the initiator holds, as much of it as the cleanup needs.
struct HeldSession {
    id: ISCSI_UNIQUE_SESSION_ID,
    target: String,
    /// Where its connections go. Empty for a session that has lost them all
    /// and is trying to reconnect.
    portals: Vec<(String, u16)>,
}

/// # Safety
/// `p` is null or points at a NUL-terminated UTF-16 string.
unsafe fn wide(p: windows::core::PWSTR) -> String {
    if p.is_null() {
        String::new()
    } else {
        p.to_string().unwrap_or_default()
    }
}

/// Every session the initiator holds right now.
fn held_sessions() -> Result<Vec<HeldSession>> {
    // u64, not u8: the buffer holds structs with pointers in them.
    let mut buf: Vec<u64> = Vec::new();
    let mut size = 0u32;
    let mut count = 0u32;
    for _ in 0..4 {
        let ptr = if buf.is_empty() {
            std::ptr::null_mut()
        } else {
            buf.as_mut_ptr() as *mut ISCSI_SESSION_INFOW
        };
        // SAFETY: `size` is the byte length of `buf` (zero with a null buffer,
        // which is how the size is asked for).
        let code = unsafe { GetIScsiSessionListW(&mut size, &mut count, ptr) };
        match code {
            0 => break,
            // A session can appear between the two calls: ask again.
            ERROR_INSUFFICIENT_BUFFER => buf = vec![0u64; (size as usize).div_ceil(8).max(1)],
            other => {
                return Err(dev(format!("iSCSI: GetIScsiSessionList failed with {}", describe(other))))
            }
        }
    }
    if buf.is_empty() || count == 0 {
        return Ok(Vec::new());
    }
    // SAFETY: the call succeeded, so the buffer starts with `count` session
    // records whose pointers all point back into the same buffer.
    let infos = unsafe {
        std::slice::from_raw_parts(buf.as_ptr() as *const ISCSI_SESSION_INFOW, count as usize)
    };
    let mut out = Vec::with_capacity(infos.len());
    for info in infos {
        let connections: &[ISCSI_CONNECTION_INFOW] =
            if info.Connections.is_null() || info.ConnectionCount == 0 {
                &[]
            } else {
                // SAFETY: as above — `ConnectionCount` records inside `buf`.
                unsafe { std::slice::from_raw_parts(info.Connections, info.ConnectionCount as usize) }
            };
        out.push(HeldSession {
            id: info.SessionId,
            // SAFETY: pointers into `buf`, NUL-terminated by the initiator.
            target: unsafe { wide(info.TargetName) },
            portals: connections
                .iter()
                .map(|c| (unsafe { wide(c.TargetAddress) }, c.TargetSocket))
                .collect(),
        });
    }
    Ok(out)
}

/// Does anything still serve this session? A target of ours lives exactly as
/// long as the process serving it, so a port nobody answers on means the
/// session is an orphan. `skip` is the target about to be logged in to: it
/// answers, but it is not what the old session was talking to.
fn target_answers(portals: &[(String, u16)], skip: (&str, u16)) -> bool {
    portals.iter().any(|(address, port)| {
        if (address.as_str(), *port) == skip {
            return false;
        }
        let Ok(ip) = address.parse::<IpAddr>() else {
            return false;
        };
        TcpStream::connect_timeout(&SocketAddr::new(ip, *port), TARGET_ANSWER_TIMEOUT).is_ok()
    })
}

/// Sessions to `target` that nothing serves any more, logged out before a new
/// login — which the initiator would otherwise refuse outright with
/// `ISDSC_TARGET_ALREADY_LOGGED_IN`. A session whose target still answers is a
/// live mount and is left alone.
fn clear_stale_sessions(target: &str, ours: (&str, u16)) -> Result<()> {
    let sessions = match held_sessions() {
        Ok(s) => s,
        Err(e) => {
            // Not knowing is not a reason to refuse the mount: the login says
            // for itself if something is in the way.
            warn!(error = %e, "iSCSI: could not list the initiator's sessions");
            return Ok(());
        }
    };
    for mut s in sessions.into_iter().filter(|s| s.target.eq_ignore_ascii_case(target)) {
        if target_answers(&s.portals, ours) {
            return Err(dev(tr!("iscsi_win.image_already_attached")).tap(target));
        }
        warn!(
            target,
            portals = ?s.portals,
            "iSCSI: a session to this image is left over from an earlier run and nothing serves \
             it any more — logging it out"
        );
        if !logout_session(&mut s.id, target) {
            return Err(dev(tr!("iscsi_win.stale_session_busy")).tap(target));
        }
        info!(target, "iSCSI: the left-over session is gone");
    }
    Ok(())
}

/// At service start: every session to an image of ours that nothing serves any
/// more. A mount lives in the process that serves it, so after an update or a
/// crash its session is an orphan — a dead disk in Disk Management, and the
/// image's next mount refused. Sessions whose target still answers (a mount
/// the CLI is serving) are left alone. Returns how many were logged out.
///
/// The initiator service is not started for this: if it is not running, it
/// holds no sessions.
pub fn clear_orphaned_sessions() -> usize {
    if !status().running {
        return 0;
    }
    let sessions = match held_sessions() {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "iSCSI: could not list the initiator's sessions");
            return 0;
        }
    };
    let prefix = crate::iscsi::IQN_PREFIX.to_ascii_lowercase();
    let mut cleared = 0;
    for mut s in sessions {
        if !s.target.to_ascii_lowercase().starts_with(&prefix) || target_answers(&s.portals, ("", 0)) {
            continue;
        }
        warn!(target = %s.target, portals = ?s.portals, "iSCSI: orphaned session of an earlier run — logging it out");
        if logout_session(&mut s.id, &s.target) {
            cleared += 1;
        }
    }
    cleared
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every code the initiator can return is named, out of the SDK's own
    /// header — including the one that cost a live run (0xefff003c).
    #[test]
    fn the_initiators_own_status_numbers_are_translated() {
        let no_portal = describe(0xEFFF_003C);
        assert!(no_portal.contains("ISDSC_NO_PORTAL_SPECIFIED"), "{no_portal}");
        assert!(no_portal.contains("no target portal"), "{no_portal}");
        assert!(describe(0xEFFF_0003).contains("ISDSC_CONNECTION_FAILED"));
        assert!(describe(0x0000_0057).contains("недопустимый параметр"));
        let unknown = describe(0xEFFF_7777);
        assert!(unknown.contains("0xefff7777") && unknown.contains("iscsierr.h"), "{unknown}");
    }

    /// Zero is a real initiator port number, not "any" — the mistake that made
    /// every portal registration fail with ERROR_INVALID_PARAMETER.
    #[test]
    fn any_initiator_port_is_not_zero() {
        assert_eq!(ANY_INITIATOR_PORT, 0xFFFF_FFFF);
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
