use tauri::State;

use crate::state::GuiState;

#[derive(serde::Serialize)]
pub struct ScheduleInfo {
    pub cron_expression: Option<String>,
    pub next_run: Option<String>,
}

#[tauri::command]
pub async fn get_schedule(state: State<'_, GuiState>, job_id: String) -> Result<ScheduleInfo, String> {
    let uuid = uuid::Uuid::parse_str(&job_id).map_err(|e| format!("Invalid job ID: {}", e))?;
    let conn = state.open_db()?;
    let job = own_backup_metadata::get_job(&conn, &uuid)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "Job not found".to_string())?;

    let next_run = job.schedule.as_ref().and_then(|expr| {
        own_backup_scheduler::parse_schedule(expr)
            .ok()
            .and_then(|s| own_backup_scheduler::next_run(&s))
            .map(|t| t.to_rfc3339())
    });

    Ok(ScheduleInfo {
        cron_expression: job.schedule,
        next_run,
    })
}

#[tauri::command]
pub async fn set_schedule(
    state: State<'_, GuiState>,
    job_id: String,
    cron_expression: String,
) -> Result<(), String> {
    if !cron_expression.is_empty() {
        own_backup_scheduler::parse_schedule(&cron_expression)
            .map_err(|e| format!("Invalid cron expression: {}", e))?;
    }

    let uuid = uuid::Uuid::parse_str(&job_id).map_err(|e| format!("Invalid job ID: {}", e))?;
    let conn = state.open_db()?;
    let mut job = own_backup_metadata::get_job(&conn, &uuid)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "Job not found".to_string())?;

    job.schedule = if cron_expression.is_empty() {
        None
    } else {
        Some(cron_expression)
    };
    job.updated_at = chrono::Utc::now();

    own_backup_metadata::update_job(&conn, &job).map_err(|e| e.to_string())
}
