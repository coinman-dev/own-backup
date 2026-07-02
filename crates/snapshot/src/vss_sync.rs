//! Synchronous VSS snapshot session for the backup engine.
//!
//! Runs the full VSS requester sequence (non-component, volume-level backup):
//!
//! 1.  CoInitializeEx (MTA)
//! 2.  CreateVssBackupComponents
//! 3.  InitializeForBackup
//! 4.  SetContext(VSS_CTX_BACKUP) — auto-release, non-persistent snapshot
//! 5.  SetBackupState(select_components=false, bootable_state=true, VSS_BT_FULL)
//! 6.  GatherWriterMetadata + wait, FreeWriterMetadata
//! 7.  StartSnapshotSet
//! 8.  AddToSnapshotSet(volume)
//! 9.  PrepareForBackup + wait
//! 10. DoSnapshotSet + wait  ← freeze/thaw point
//! 11. GetSnapshotProperties → shadow device path
//!
//! After copying blocks the engine calls [`VssSnapshotSession::complete`]
//! (BackupComplete + wait). Dropping an uncompleted session aborts the backup;
//! releasing the components object deletes the auto-release snapshot either way.
//!
//! ⚠️ Thread affinity: create, use and drop a session on ONE thread (the
//! engine runs the whole backup inside a single blocking task).

#![cfg(target_os = "windows")]

use std::ffi::c_void;
use std::ptr::null_mut;

use tracing::{debug, info, warn};
use windows::core::{Interface, GUID, HRESULT};
use windows::Win32::Storage::Vss::{IVssAsync, VSS_BT_FULL, VSS_CTX_BACKUP, VSS_SNAPSHOT_PROP};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use own_backup_common::{OwnBackupError, Result};

use crate::vss_raw::{IVssBackupComponentsRaw, VssApi, VSS_S_ASYNC_FINISHED};

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn check(hr: HRESULT, what: &str) -> Result<()> {
    if hr.is_ok() {
        Ok(())
    } else {
        Err(OwnBackupError::Vss(format!("{what} failed: {hr:?} {}", hr.message())))
    }
}

/// Initialize COM (MTA) for the current thread. `S_FALSE` (already
/// initialized) is fine; `RPC_E_CHANGED_MODE` is downgraded to a warning.
fn initialize_com() -> Result<()> {
    // SAFETY: standard COM initialization.
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if hr.is_err() {
        // 0x80010106 = RPC_E_CHANGED_MODE: thread already STA — VSS usually
        // still works; log and continue.
        warn!(hr = ?hr, "CoInitializeEx did not return S_OK");
    }
    Ok(())
}

/// Wait for an `IVssAsync` operation and verify it finished successfully.
///
/// Takes ownership of the raw pointer returned by the vtable call.
fn wait_async(ptr: *mut c_void, what: &str) -> Result<()> {
    if ptr.is_null() {
        return Err(OwnBackupError::Vss(format!("{what}: null IVssAsync")));
    }
    // SAFETY: ptr is a valid IVssAsync we own; from_raw takes over the reference.
    let async_op = unsafe { IVssAsync::from_raw(ptr) };
    // SAFETY: interface pointer is valid.
    unsafe { async_op.Wait(u32::MAX) }
        .map_err(|e| OwnBackupError::Vss(format!("{what}: Wait failed: {e}")))?;

    let mut status = HRESULT(0);
    // SAFETY: out pointer valid.
    unsafe { async_op.QueryStatus(&mut status, std::ptr::null_mut()) }
        .map_err(|e| OwnBackupError::Vss(format!("{what}: QueryStatus failed: {e}")))?;

    if status != VSS_S_ASYNC_FINISHED {
        return Err(OwnBackupError::Vss(format!(
            "{what}: async status {status:?} (expected VSS_S_ASYNC_FINISHED)"
        )));
    }
    debug!(what, "VSS async step finished");
    Ok(())
}

/// A live VSS shadow copy of one volume.
pub struct VssSnapshotSession {
    components: *mut IVssBackupComponentsRaw,
    api: VssApi,
    snapshot_set_id: GUID,
    snapshot_id: GUID,
    device_path: String,
    completed: bool,
}

macro_rules! com_call {
    ($self:expr, $method:ident, $what:expr $(, $arg:expr)*) => {{
        // SAFETY: components points to a live COM object; vtable layout is
        // verified against vsbackup.h in vss_raw.rs.
        let hr = unsafe { ((*(*$self.components).vtbl).$method)($self.components as *mut c_void $(, $arg)*) };
        check(hr, $what)
    }};
}

impl VssSnapshotSession {
    /// Create a shadow copy of `volume_root` (e.g. `C:\` — trailing backslash
    /// required by VSS).
    pub fn create(volume_root: &str) -> Result<Self> {
        info!(volume = volume_root, "Creating VSS shadow copy");
        initialize_com()?;
        let api = VssApi::load()?;

        let mut components: *mut IVssBackupComponentsRaw = null_mut();
        // SAFETY: documented entry point; out pointer is valid.
        let hr = unsafe { (api.create_backup_components)(&mut components) };
        check(hr, "CreateVssBackupComponents")?;
        if components.is_null() {
            return Err(OwnBackupError::Vss("CreateVssBackupComponents returned null".into()));
        }

        let mut session = Self {
            components,
            api,
            snapshot_set_id: GUID::zeroed(),
            snapshot_id: GUID::zeroed(),
            device_path: String::new(),
            completed: false,
        };

        session.run_sequence(volume_root)?;
        Ok(session)
    }

    fn run_sequence(&mut self, volume_root: &str) -> Result<()> {
        com_call!(self, initialize_for_backup, "InitializeForBackup", std::ptr::null())?;
        com_call!(self, set_context, "SetContext", VSS_CTX_BACKUP.0)?;
        com_call!(
            self,
            set_backup_state,
            "SetBackupState",
            0u8, // no component selection — whole-volume backup
            1u8, // include bootable system state
            VSS_BT_FULL.0,
            0u8
        )?;

        let mut async_ptr: *mut c_void = null_mut();
        com_call!(self, gather_writer_metadata, "GatherWriterMetadata", &mut async_ptr)?;
        wait_async(async_ptr, "GatherWriterMetadata")?;
        com_call!(self, free_writer_metadata, "FreeWriterMetadata")?;

        com_call!(self, start_snapshot_set, "StartSnapshotSet", &mut self.snapshot_set_id)?;
        debug!(set_id = ?self.snapshot_set_id, "Snapshot set started");

        let volume_wide = to_wide(volume_root);
        com_call!(
            self,
            add_to_snapshot_set,
            "AddToSnapshotSet",
            volume_wide.as_ptr(),
            GUID::zeroed(), // default provider
            &mut self.snapshot_id
        )?;

        let mut async_ptr: *mut c_void = null_mut();
        com_call!(self, prepare_for_backup, "PrepareForBackup", &mut async_ptr)?;
        wait_async(async_ptr, "PrepareForBackup")?;

        // Freeze/thaw point — this is where the shadow copy is actually taken.
        let mut async_ptr: *mut c_void = null_mut();
        com_call!(self, do_snapshot_set, "DoSnapshotSet", &mut async_ptr)?;
        wait_async(async_ptr, "DoSnapshotSet")?;

        // Fetch the shadow device path.
        let mut prop = VSS_SNAPSHOT_PROP::default();
        com_call!(
            self,
            get_snapshot_properties,
            "GetSnapshotProperties",
            self.snapshot_id,
            &mut prop
        )?;
        // SAFETY: prop was filled by a successful call; the string is valid
        // until VssFreeSnapshotProperties.
        self.device_path = unsafe {
            windows::core::PWSTR(prop.m_pwszSnapshotDeviceObject)
                .to_string()
                .unwrap_or_default()
        };
        // SAFETY: frees the strings inside prop exactly once.
        unsafe { (self.api.free_snapshot_properties)(&mut prop) };

        if self.device_path.is_empty() {
            return Err(OwnBackupError::Vss("empty snapshot device path".into()));
        }

        info!(
            device = %self.device_path,
            snapshot_id = ?self.snapshot_id,
            "VSS shadow copy ready"
        );
        Ok(())
    }

    /// Read-only device path of the shadow copy
    /// (`\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopyN`).
    pub fn device_path(&self) -> &str {
        &self.device_path
    }

    /// Notify writers that the backup succeeded, then release the snapshot.
    pub fn complete(mut self) -> Result<()> {
        let mut async_ptr: *mut c_void = null_mut();
        com_call!(self, backup_complete, "BackupComplete", &mut async_ptr)?;
        wait_async(async_ptr, "BackupComplete")?;
        self.completed = true;
        info!("VSS backup completed, releasing shadow copy");
        // Drop releases the components object; the auto-release snapshot
        // (VSS_CTX_BACKUP) is deleted by VSS on release.
        Ok(())
    }
}

impl Drop for VssSnapshotSession {
    fn drop(&mut self) {
        if self.components.is_null() {
            return;
        }
        if !self.completed {
            // Backup did not finish — tell writers it was aborted.
            // SAFETY: vtable slot verified; object still alive.
            let hr = unsafe {
                ((*(*self.components).vtbl).abort_backup)(self.components as *mut c_void)
            };
            if hr.is_err() {
                warn!(hr = ?hr, "AbortBackup failed during drop");
            }
        }
        // SAFETY: releasing our only reference; snapshot auto-deletes.
        unsafe { ((*(*self.components).vtbl).release)(self.components as *mut c_void) };
        self.components = null_mut();
        debug!("IVssBackupComponents released");
    }
}
