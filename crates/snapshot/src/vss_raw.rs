//! Raw FFI bindings for the VSS requester API (`vsbackup.h`).
//!
//! `IVssBackupComponents` is NOT exposed by the `windows` crate (it lives in
//! vsbackup.h / VssApi.dll, outside the Win32 metadata), so the COM vtable is
//! declared by hand. The slot order below was verified against the
//! mingw-w64 copy of `vsbackup.h` (48 virtual methods after IUnknown) —
//! **do not reorder fields**: a wrong slot means calling a random function.
//!
//! Only the methods we actually invoke get typed signatures; every other slot
//! is declared as `usize` padding.
//!
//! Creation goes through `CreateVssBackupComponentsInternal`, exported by
//! `VssApi.dll` (the documented `CreateVssBackupComponents` is a static-lib
//! shim around it). We resolve it at runtime via LoadLibrary/GetProcAddress
//! to keep cross-compilation trivial.

#![cfg(target_os = "windows")]

use std::ffi::c_void;

use windows::core::{s, w, GUID, HRESULT};
use windows::Win32::Storage::Vss::VSS_SNAPSHOT_PROP;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

use own_backup_common::{OwnBackupError, Result};

/// `IVssAsync::QueryStatus` value: operation finished successfully.
pub const VSS_S_ASYNC_FINISHED: HRESULT = HRESULT(0x0004230Au32 as i32);

/// C++ `bool` parameter (1 byte).
pub type CppBool = u8;

/// Vtable of `IVssBackupComponents` (after the 3 IUnknown slots).
/// Order verified against vsbackup.h — 48 methods.
#[repr(C)]
pub struct IVssBackupComponentsVtbl {
    // --- IUnknown ---
    pub query_interface: usize,
    pub add_ref: unsafe extern "system" fn(this: *mut c_void) -> u32,
    pub release: unsafe extern "system" fn(this: *mut c_void) -> u32,
    // --- IVssBackupComponents (declaration order from vsbackup.h) ---
    pub get_writer_components_count: usize, // 1
    pub get_writer_components: usize,       // 2
    /// 3: InitializeForBackup(BSTR bstrXML = NULL)
    pub initialize_for_backup: unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT,
    /// 4: SetBackupState(bool, bool, VSS_BACKUP_TYPE, bool)
    pub set_backup_state:
        unsafe extern "system" fn(*mut c_void, CppBool, CppBool, i32, CppBool) -> HRESULT,
    pub initialize_for_restore: usize, // 5
    pub set_restore_state: usize,      // 6
    /// 7: GatherWriterMetadata(IVssAsync **)
    pub gather_writer_metadata:
        unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
    pub get_writer_metadata_count: usize, // 8
    pub get_writer_metadata: usize,       // 9
    /// 10: FreeWriterMetadata()
    pub free_writer_metadata: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    pub add_component: usize, // 11
    /// 12: PrepareForBackup(IVssAsync **)
    pub prepare_for_backup: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
    /// 13: AbortBackup()
    pub abort_backup: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    pub gather_writer_status: usize,    // 14
    pub get_writer_status_count: usize, // 15
    pub free_writer_status: usize,      // 16
    pub get_writer_status: usize,       // 17
    pub set_backup_succeeded: usize,    // 18
    pub set_backup_options: usize,      // 19
    pub set_selected_for_restore: usize, // 20
    pub set_restore_options: usize,     // 21
    pub set_additional_restores: usize, // 22
    pub set_previous_backup_stamp: usize, // 23
    pub save_as_xml: usize,             // 24
    /// 25: BackupComplete(IVssAsync **)
    pub backup_complete: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
    pub add_alternative_location_mapping: usize, // 26
    pub add_restore_subcomponent: usize,         // 27
    pub set_file_restore_status: usize,          // 28
    pub add_new_target: usize,                   // 29
    pub set_ranges_file_path: usize,             // 30
    pub pre_restore: usize,                      // 31
    pub post_restore: usize,                     // 32
    /// 33: SetContext(LONG)
    pub set_context: unsafe extern "system" fn(*mut c_void, i32) -> HRESULT,
    /// 34: StartSnapshotSet(VSS_ID *)
    pub start_snapshot_set: unsafe extern "system" fn(*mut c_void, *mut GUID) -> HRESULT,
    /// 35: AddToSnapshotSet(VSS_PWSZ, VSS_ID, VSS_ID *)
    pub add_to_snapshot_set:
        unsafe extern "system" fn(*mut c_void, *const u16, GUID, *mut GUID) -> HRESULT,
    /// 36: DoSnapshotSet(IVssAsync **)
    pub do_snapshot_set: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
    /// 37: DeleteSnapshots(VSS_ID, VSS_OBJECT_TYPE, BOOL, LONG *, VSS_ID *)
    pub delete_snapshots:
        unsafe extern "system" fn(*mut c_void, GUID, i32, i32, *mut i32, *mut GUID) -> HRESULT,
    pub import_snapshots: usize,  // 38
    pub break_snapshot_set: usize, // 39
    /// 40: GetSnapshotProperties(VSS_ID, VSS_SNAPSHOT_PROP *)
    pub get_snapshot_properties:
        unsafe extern "system" fn(*mut c_void, GUID, *mut VSS_SNAPSHOT_PROP) -> HRESULT,
    pub query: usize, // 41
    /// 42: IsVolumeSupported(VSS_ID, VSS_PWSZ, BOOL *)
    pub is_volume_supported:
        unsafe extern "system" fn(*mut c_void, GUID, *const u16, *mut i32) -> HRESULT,
    pub disable_writer_classes: usize,   // 43
    pub enable_writer_classes: usize,    // 44
    pub disable_writer_instances: usize, // 45
    pub expose_snapshot: usize,          // 46
    pub revert_to_snapshot: usize,       // 47
    pub query_revert_status: usize,      // 48
}

/// A raw `IVssBackupComponents` COM object pointer.
#[repr(C)]
pub struct IVssBackupComponentsRaw {
    pub vtbl: *const IVssBackupComponentsVtbl,
}

pub type CreateVssBackupComponentsFn =
    unsafe extern "system" fn(*mut *mut IVssBackupComponentsRaw) -> HRESULT;
pub type VssFreeSnapshotPropertiesFn = unsafe extern "system" fn(*mut VSS_SNAPSHOT_PROP);

/// Entry points resolved from VssApi.dll.
#[derive(Clone, Copy)]
pub struct VssApi {
    pub create_backup_components: CreateVssBackupComponentsFn,
    pub free_snapshot_properties: VssFreeSnapshotPropertiesFn,
}

impl VssApi {
    /// Load VssApi.dll and resolve the internal entry points.
    ///
    /// The module handle is intentionally leaked — VSS stays loaded for the
    /// lifetime of the process.
    pub fn load() -> Result<Self> {
        // SAFETY: standard LoadLibrary/GetProcAddress dance with static strings.
        unsafe {
            let module = LoadLibraryW(w!("VssApi.dll")).map_err(|e| {
                OwnBackupError::Vss(format!("LoadLibrary(VssApi.dll) failed: {e}"))
            })?;

            let create = GetProcAddress(module, s!("CreateVssBackupComponentsInternal"))
                .ok_or_else(|| {
                    OwnBackupError::Vss(
                        "CreateVssBackupComponentsInternal not found in VssApi.dll".into(),
                    )
                })?;
            let free_props = GetProcAddress(module, s!("VssFreeSnapshotPropertiesInternal"))
                .ok_or_else(|| {
                    OwnBackupError::Vss(
                        "VssFreeSnapshotPropertiesInternal not found in VssApi.dll".into(),
                    )
                })?;

            Ok(Self {
                create_backup_components: std::mem::transmute::<
                    unsafe extern "system" fn() -> isize,
                    CreateVssBackupComponentsFn,
                >(create),
                free_snapshot_properties: std::mem::transmute::<
                    unsafe extern "system" fn() -> isize,
                    VssFreeSnapshotPropertiesFn,
                >(free_props),
            })
        }
    }
}
