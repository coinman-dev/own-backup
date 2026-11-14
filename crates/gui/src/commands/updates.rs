use chrono::{DateTime, Months, Utc};
use own_backup_backup::media_linux::{github_releases_url, GITHUB_REPO};
use own_backup_common::config::{SettingsPatch, UpdateCheckInterval};
use semver::Version;
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::state::GuiState;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UpdateCheckDto {
    pub status: &'static str,
    pub current_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_url: Option<String>,
}

impl UpdateCheckDto {
    fn new(status: &'static str, current_version: &Version) -> Self {
        Self {
            status,
            current_version: current_version.to_string(),
            latest_version: None,
            release_url: None,
        }
    }

    fn available(current_version: &Version, latest: Version, tag: String) -> Self {
        Self {
            status: "available",
            current_version: current_version.to_string(),
            latest_version: Some(latest.to_string()),
            release_url: Some(format!(
                "https://github.com/{GITHUB_REPO}/releases/tag/{tag}"
            )),
        }
    }
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
}

fn is_due(interval: UpdateCheckInterval, last: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    let Some(last) = last else {
        return interval != UpdateCheckInterval::Disabled;
    };
    if last > now {
        return true;
    }
    match interval {
        UpdateCheckInterval::Disabled => false,
        UpdateCheckInterval::Weekly => now.signed_duration_since(last) >= chrono::Duration::days(7),
        UpdateCheckInterval::Monthly => last
            .checked_add_months(Months::new(1))
            .map_or(true, |next| now >= next),
    }
}

fn newest_release(current: &Version, releases: &[GithubRelease]) -> Option<(Version, String)> {
    releases
        .iter()
        .filter(|release| !release.draft)
        .filter_map(|release| {
            let tag = release
                .tag_name
                .strip_prefix('v')
                .unwrap_or(&release.tag_name);
            let version = Version::parse(tag).ok()?;
            (version > *current && (!current.pre.is_empty() || version.pre.is_empty()))
                .then_some((version, release.tag_name.clone()))
        })
        .max_by(|(a, _), (b, _)| a.cmp(b))
}

fn release_to_dto(current: &Version, releases: &[GithubRelease]) -> UpdateCheckDto {
    match newest_release(current, releases) {
        Some((version, tag)) => UpdateCheckDto::available(current, version, tag),
        None => UpdateCheckDto::new("current", current),
    }
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn fetch_releases() -> Result<Vec<GithubRelease>, String> {
    #[cfg(target_os = "windows")]
    use std::path::Path;
    use std::process::{Command, Stdio};

    #[cfg(target_os = "linux")]
    let mut command = Command::new("curl");
    #[cfg(target_os = "windows")]
    let mut command = {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        let mut command = Command::new(Path::new(&root).join("System32").join("curl.exe"));
        command.creation_flags(CREATE_NO_WINDOW);
        command.arg("--ssl-revoke-best-effort");
        command
    };
    let output = command
        .env("LC_ALL", "C")
        .args([
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--connect-timeout",
            "10",
            "--max-time",
            "30",
            "--max-filesize",
            "16M",
            "--user-agent",
        ])
        .arg(format!("own-backup/{}", env!("CARGO_PKG_VERSION")))
        .args(["-H", "Accept: application/vnd.github+json"])
        .arg(github_releases_url())
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot start curl: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "GitHub update check failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|e| format!("invalid GitHub response: {e}"))
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn fetch_releases() -> Result<Vec<GithubRelease>, String> {
    Err("update checks are supported on Windows and Linux".into())
}

fn check_now(current: Version) -> Result<UpdateCheckDto, String> {
    let releases = fetch_releases()?;
    Ok(release_to_dto(&current, &releases))
}

#[tauri::command]
pub async fn check_for_updates(
    state: State<'_, GuiState>,
    force: bool,
) -> Result<UpdateCheckDto, String> {
    let current = Version::parse(env!("CARGO_PKG_VERSION")).map_err(|e| e.to_string())?;
    let mut config = state.config.lock().map_err(|e| e.to_string())?.clone();
    if let Some(mut client) = state.link.connected().await {
        if let Ok(fresh) = client.get_settings().await {
            config = fresh;
        }
    }
    if own_backup_common::media_mode::is_media_mode()
        || (!force && config.update_check_interval == UpdateCheckInterval::Disabled)
    {
        return Ok(UpdateCheckDto::new("disabled", &current));
    }
    if !force
        && !is_due(
            config.update_check_interval,
            config.last_update_check,
            Utc::now(),
        )
    {
        return Ok(UpdateCheckDto::new("not_due", &current));
    }

    let checked = Utc::now();
    let outcome = tokio::task::spawn_blocking(move || check_now(current))
        .await
        .map_err(|e| e.to_string())?;
    config.last_update_check = Some(checked);
    if let Some(mut client) = state.link.connected().await {
        let patch = SettingsPatch {
            last_update_check: Some(Some(checked)),
            ..Default::default()
        };
        match client.save_settings(&patch).await {
            Ok(saved) => config = saved,
            Err(error) => tracing::info!(%error, "update check time was not saved by the service"),
        }
    }
    *state.config.lock().map_err(|e| e.to_string())? = config;
    outcome
}

#[tauri::command]
pub fn open_update_release(url: String) -> Result<(), String> {
    let prefix = format!("https://github.com/{GITHUB_REPO}/releases/tag/");
    if !url.starts_with(&prefix) {
        return Err("not an Own Backup release URL".into());
    }
    #[cfg(target_os = "windows")]
    {
        use windows::core::{w, PCWSTR};
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

        let url: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
        let result = unsafe {
            ShellExecuteW(
                None,
                w!("open"),
                PCWSTR(url.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            )
        };
        if result.0 as usize <= 32 {
            return Err(format!(
                "Windows could not open the release page: {}",
                result.0 as usize
            ));
        }
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("cannot open the release page: {e}"))
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = url;
        Err("opening a release page is supported on Windows and Linux".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn release(tag: &str) -> GithubRelease {
        GithubRelease {
            tag_name: tag.into(),
            draft: false,
        }
    }

    #[test]
    fn weekly_and_monthly_checks_wait_for_their_interval() {
        let now = Utc.with_ymd_and_hms(2026, 11, 14, 12, 0, 0).unwrap();
        assert!(!is_due(UpdateCheckInterval::Weekly, Some(now), now));
        assert!(is_due(
            UpdateCheckInterval::Weekly,
            Some(now - chrono::Duration::days(7)),
            now,
        ));
        assert!(!is_due(
            UpdateCheckInterval::Monthly,
            Some(Utc.with_ymd_and_hms(2026, 10, 15, 12, 0, 0).unwrap()),
            now,
        ));
        assert!(is_due(
            UpdateCheckInterval::Monthly,
            Some(Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap()),
            now,
        ));
        assert!(is_due(UpdateCheckInterval::Weekly, None, now));
        assert!(!is_due(UpdateCheckInterval::Disabled, None, now));
    }

    #[test]
    fn the_newest_newer_release_wins_not_the_first_in_the_list() {
        let current = Version::parse("0.2.1-beta").unwrap();
        let releases = vec![
            release("v0.2.2-beta"),
            release("v0.3.0-beta"),
            release("v0.2.1-beta"),
        ];
        let dto = release_to_dto(&current, &releases);
        assert_eq!(dto.status, "available");
        assert_eq!(dto.latest_version.as_deref(), Some("0.3.0-beta"));
        assert_eq!(
            dto.release_url.as_deref(),
            Some("https://github.com/coinman-dev/own-backup/releases/tag/v0.3.0-beta")
        );
    }

    #[test]
    fn stable_versions_are_not_offered_prereleases() {
        let stable = Version::parse("0.2.1").unwrap();
        let releases = vec![release("v0.3.0-beta")];
        assert_eq!(release_to_dto(&stable, &releases).status, "current");

        let beta = Version::parse("0.2.1-beta").unwrap();
        assert_eq!(release_to_dto(&beta, &releases).status, "available");
    }

    #[test]
    fn drafts_invalid_and_older_versions_are_ignored() {
        let current = Version::parse("0.2.1-beta").unwrap();
        let mut draft = release("v9.0.0-beta");
        draft.draft = true;
        let releases = vec![draft, release("not-a-version"), release("v0.2.0-beta")];
        assert_eq!(release_to_dto(&current, &releases).status, "current");
    }
}
