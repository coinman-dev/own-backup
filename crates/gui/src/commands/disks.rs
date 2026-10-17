use own_backup_common::disk::DiskInfo;
use tauri::State;

use crate::state::GuiState;

/// Disk inventory comes from the service: enumerating disks and probing raw
/// superblocks of letterless partitions needs admin, which the LocalSystem
/// service has and the (now non-elevated) GUI does not.
#[tauri::command]
pub async fn list_disks(state: State<'_, GuiState>) -> Result<Vec<DiskInfo>, String> {
    let json = state.link.client().await?.list_disks_json().await?;
    let disks: Vec<DiskInfo> =
        serde_json::from_str(&json).map_err(|e| format!("bad disk list from service: {e}"))?;
    tracing::info!(
        disks = disks.len(),
        volumes = disks
            .iter()
            .flat_map(|d| &d.partitions)
            .filter(|p| p.volume_letter.is_some())
            .count(),
        "list_disks ok (via service)"
    );
    Ok(disks)
}

/// Cheap signature of present drive letters — the frontend polls this to
/// detect USB insert/eject and re-enumerate only when it actually changes.
#[tauri::command]
pub async fn drive_signature() -> Result<u32, String> {
    Ok(own_backup_inventory::drive_letters_signature())
}

/// EFI, recovery and vendor partitions Windows keeps without a letter — the
/// list on the «Инструменты» page. The service decides what belongs on it.
#[tauri::command]
pub async fn list_hidden_volumes(
    state: State<'_, GuiState>,
) -> Result<Vec<own_backup_common::disk::HiddenVolume>, String> {
    state.link.client().await?.list_hidden_volumes().await
}

/// Give a hidden partition a drive letter, or take it away (`letter: null`).
#[tauri::command]
pub async fn set_volume_letter(
    state: State<'_, GuiState>,
    device: String,
    letter: Option<char>,
) -> Result<Vec<own_backup_common::disk::HiddenVolume>, String> {
    state
        .link
        .client()
        .await?
        .set_volume_letter(&device, letter)
        .await
}
