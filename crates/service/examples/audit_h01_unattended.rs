//! Disposable Windows VM probe: save a validated BitLocker password through
//! the real service RPC and arm a startup-triggered job for the next boot.
use own_backup_client::BackupClient;
use own_backup_common::config::AppConfig;
use own_backup_common::types::ImageFormat;

const PASSWORD: &str = "Str0ng-Lab-Passphrase-2026";
const JOB_ID_FILE: &str = r"C:\Users\lab\AppData\Local\audit-h01-unattended-job-id.txt";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = AppConfig::default();
    let token = std::fs::read_to_string(config.token_path())?;
    let mut client = BackupClient::connect(config.grpc_address.clone(), token.trim().to_string()).await?;
    match std::env::args().nth(1).as_deref() {
        Some("prepare") => {
            let volume = std::env::args().nth(2).ok_or("volume argument missing")?;
            let dest = r"C:\Lab\audit-h01-unattended-images";
            let unlocked = client
                .provide_volume_password(&volume, PASSWORD, true)
                .await?;
            if !unlocked.ok {
                return Err("service did not unlock and validate BitLocker password".into());
            }
            println!("H01-UNATTENDED-PASSWORD saved=true filesystem={}", unlocked.filesystem);
            let mut job = client.new_job_template().await?;
            job.name = "AuditH01Unattended".into();
            job.source = volume.clone();
            job.sources = vec![volume.clone()];
            job.source_label = "H01BL".into();
            job.source_labels = vec!["H01BL".into()];
            job.requires_password = true;
            job.dest_folder = dest.into();
            job.image_format = ImageFormat::Qcow2;
            job.schedule.kind = "none".into();
            job.schedule.event_trigger = "startup".into();
            job.schedule.event_once_per_day = false;
            job.schedule.event_startup_delay_min = 0;
            job.scheme.method = "full".into();
            job.exclusions.patterns.clear();
            let saved = client.save_job(&job).await?;
            std::fs::write(JOB_ID_FILE, &saved.id)?;
            println!("H01-UNATTENDED-JOB id={} source={volume}", saved.id);
        }
        Some("arm") => {
            let id = std::fs::read_to_string(JOB_ID_FILE)?;
            let mut job = client.get_job(id.trim()).await?.ok_or("job missing")?;
            job.schedule.kind = "event".into();
            job.schedule.event_trigger = "startup".into();
            job.schedule.event_startup_delay_min = 0;
            job.schedule.event_once_per_day = false;
            let saved = client.save_job(&job).await?;
            println!("H01-UNATTENDED-ARMED id={}", saved.id);
        }
        _ => return Err("usage: audit_h01_unattended.exe prepare <volume>|arm".into()),
    }
    Ok(())
}
