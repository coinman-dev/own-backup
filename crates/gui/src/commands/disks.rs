use own_backup_common::disk::DiskInfo;
use own_backup_inventory::DiskInventory;
use tauri::State;

use crate::state::GuiState;

#[tauri::command]
pub async fn list_disks(_state: State<'_, GuiState>) -> Result<Vec<DiskInfo>, String> {
    let result: Result<Vec<DiskInfo>, String>;

    #[cfg(target_os = "windows")]
    {
        result = match own_backup_inventory::WmiInventory::new() {
            Ok(inventory) => inventory
                .enumerate_disks()
                .await
                .map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
    }

    #[cfg(not(target_os = "windows"))]
    {
        let inventory = own_backup_inventory::MockInventory::default();
        result = inventory.enumerate_disks().await.map_err(|e| e.to_string());
    }

    match &result {
        Ok(disks) => tracing::info!(
            disks = disks.len(),
            volumes = disks
                .iter()
                .flat_map(|d| &d.partitions)
                .filter(|p| p.volume_letter.is_some())
                .count(),
            "list_disks ok"
        ),
        Err(e) => tracing::error!(error = %e, "list_disks failed"),
    }
    result
}

/// Cheap signature of present drive letters — the frontend polls this to
/// detect USB insert/eject and re-enumerate only when it actually changes.
#[tauri::command]
pub async fn drive_signature() -> Result<u32, String> {
    Ok(own_backup_inventory::drive_letters_signature())
}
