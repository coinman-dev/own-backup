use own_backup_common::preflight::PreflightReport;
use own_backup_inventory::PreflightAnalyzer;
use tauri::State;

use crate::state::GuiState;

#[tauri::command]
pub async fn run_preflight(
    _state: State<'_, GuiState>,
    device_id: String,
) -> Result<PreflightReport, String> {
    let analyzer = PreflightAnalyzer::new();
    analyzer.analyze(&device_id).map_err(|e| e.to_string())
}
