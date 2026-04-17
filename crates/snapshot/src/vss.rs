// VSS (Volume Shadow Copy Service) snapshot provider -- Windows only.
//
// This module is compiled exclusively on Windows where we have access to
// the VSS COM interfaces via the `windows` crate.

#![cfg(target_os = "windows")]

use async_trait::async_trait;
use chrono::Utc;
use own_backup_common::{OwnBackupError, Result};
use std::collections::HashMap;
use tracing::{info, warn};

use crate::traits::{Snapshot, SnapshotProvider};
use crate::vss_com;

// Windows crate imports for VSS COM interfaces.
// NOTE: IVssBackupComponents (the main VSS Requester interface) is not yet
// exposed by windows crate 0.58. The full 12-step implementation in
// create_snapshot() uses todo!() and will require either a newer windows crate
// version or manual COM vtable bindings when this is implemented.
use windows::Win32::Storage::Vss::{
    IVssAsync, VSS_CTX_BACKUP, VSS_OBJECT_SNAPSHOT,
    VSS_SNAPSHOT_PROP, VSS_BT_FULL,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
};

/// VSS-backed snapshot provider for Windows.
///
/// Uses the VSS COM API to create and release shadow copies. Each snapshot
/// goes through the full VSS requester sequence (see `create_snapshot` for
/// the 12-step process).
pub struct VssProvider {
    _private: (),
}

impl VssProvider {
    pub fn new() -> Result<Self> {
        // Ensure COM is initialised for this thread.
        vss_com::initialize_com()?;
        Ok(VssProvider { _private: () })
    }
}

#[async_trait]
impl SnapshotProvider for VssProvider {
    fn name(&self) -> &str {
        "VSS"
    }

    async fn create_snapshot(&self, volume: &str) -> Result<Snapshot> {
        info!(volume, "Creating VSS shadow copy");

        // The full VSS requester sequence for a backup snapshot:
        //
        // Step 1:  CoCreateInstance(CLSID_VssBackupComponents) -> IVssBackupComponents
        //          Get the backup components COM object.
        //
        // Step 2:  InitializeForBackup(None)
        //          Prepare the backup components for a new backup session.
        //
        // Step 3:  SetBackupState(false, true, VSS_BT_FULL, false)
        //          Declare this is a full backup, component-mode not selected,
        //          with bootable system state.
        //
        // Step 4:  GatherWriterMetadata() -> IVssAsync
        //          Ask all registered VSS writers to report their metadata.
        //          Must wait for completion.
        //
        // Step 5:  (Optional) Enumerate writers and select components.
        //          For block-level backup we typically skip component selection
        //          and just snapshot the whole volume.
        //
        // Step 6:  StartSnapshotSet() -> VSS_ID (snapshot set GUID)
        //          Begin a new snapshot set.
        //
        // Step 7:  AddToSnapshotSet(volume, GUID_NULL) -> VSS_ID (shadow copy ID)
        //          Add the target volume to the snapshot set.
        //
        // Step 8:  PrepareForBackup() -> IVssAsync
        //          Notify writers to prepare (flush buffers, etc.).
        //          Must wait for completion.
        //
        // Step 9:  DoSnapshotSet() -> IVssAsync
        //          Actually create the shadow copy. This is the freeze/thaw
        //          point. Wait for completion.
        //
        // Step 10: GetSnapshotProperties(shadow_copy_id) -> VSS_SNAPSHOT_PROP
        //          Retrieve the device path for the new shadow copy
        //          (m_pwszSnapshotDeviceObject).
        //
        // Step 11: BackupComplete() -> IVssAsync
        //          Notify writers the backup is done. Wait for completion.
        //
        // Step 12: Build Snapshot struct with the device path, timestamps,
        //          and metadata (snapshot set ID, shadow copy ID).

        todo!(
            "Execute the 12-step VSS requester sequence for volume '{volume}'. \
             Requires running on a real Windows host with admin privileges."
        )
    }

    async fn release_snapshot(&self, snapshot: &Snapshot) -> Result<()> {
        info!(
            device_path = snapshot.snapshot_device_path.as_str(),
            "Releasing VSS shadow copy"
        );

        // To release a shadow copy:
        //
        // 1. Extract the snapshot set ID from snapshot.metadata.
        // 2. Call IVssBackupComponents::DeleteSnapshots with:
        //    - source_object_id: snapshot set GUID
        //    - object_type: VSS_OBJECT_SNAPSHOT_SET
        //    - force_delete: true
        // 3. Verify the deleted count matches expectations.
        //
        // Note: after release, the snapshot_device_path is no longer valid
        // and any open handles to it will fail.

        todo!(
            "Release VSS shadow copy at '{}'. \
             Requires running on a real Windows host.",
            snapshot.snapshot_device_path,
        )
    }
}
