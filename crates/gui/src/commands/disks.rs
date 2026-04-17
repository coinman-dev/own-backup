use own_backup_common::disk::DiskInfo;
use own_backup_inventory::DiskInventory;
use tauri::State;

use crate::state::GuiState;

#[tauri::command]
pub async fn list_disks(_state: State<'_, GuiState>) -> Result<Vec<DiskInfo>, String> {
    #[cfg(target_os = "windows")]
    {
        let inventory = own_backup_inventory::WmiInventory::new()
            .map_err(|e| e.to_string())?;
        inventory.enumerate_disks().await.map_err(|e| e.to_string())
    }

    #[cfg(not(target_os = "windows"))]
    {
        let inventory = own_backup_inventory::MockInventory::default();
        inventory.enumerate_disks().await.map_err(|e| e.to_string())
    }
}
