//! Encrypted .obk live-service probe for the disposable Windows lab.
#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().expect("prepare|mount");
    let value = args.next().expect("job id or image path");
    let token =
        std::fs::read_to_string(own_backup_common::config::AppConfig::default().token_path())
            .unwrap_or_default();
    let mut client =
        own_backup_client::BackupClient::connect("http://127.0.0.1:50051", token.trim())
            .await
            .expect("connect to service");
    match mode.as_str() {
        "prepare" => {
            let job = client.get_job(&value).await.unwrap().expect("job exists");
            let begun = client
                .begin_archive_encryption(&job, "AuditOnly!2026")
                .await
                .expect("begin encryption");
            let encrypted = client
                .confirm_archive_encryption(&begun.token, &begun.recovery_key)
                .await
                .expect("confirm encryption");
            assert!(encrypted.archive_encryption.is_some());
            client.run_job(&value).await.expect("run encrypted backup");
            for _ in 0..300 {
                let current = client.get_job(&value).await.unwrap().unwrap();
                if let Some(run) = current.last_run {
                    if run.status == "success" {
                        assert!(run.image_path.ends_with(".obk"));
                        println!("encrypted_image={}", run.image_path);
                        return;
                    }
                    if run.status == "error" {
                        panic!("encrypted backup failed: {}", run.error);
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            panic!("encrypted backup did not finish");
        }
        "mount" => {
            let info = client.inspect_image(&value).await.expect("inspect image");
            assert!(info.encrypted, "image is not encrypted");
            let no_secret = client
                .mount_image(&value, false, &own_backup_client::ArchiveSecret::none())
                .await;
            println!("without_secret_rejected={}", no_secret.is_err());
            assert!(no_secret.is_err());
            let bad = client
                .mount_image(
                    &value,
                    false,
                    &own_backup_client::ArchiveSecret::password("WrongOnly!2026"),
                )
                .await;
            println!("wrong_secret_rejected={}", bad.is_err());
            assert!(bad.is_err());
            let physical = client
                .mount_image(
                    &value,
                    false,
                    &own_backup_client::ArchiveSecret::password("AuditOnly!2026"),
                )
                .await
                .expect("mount with correct password");
            println!("mounted_physical={physical}");
        }
        _ => panic!("unknown mode {mode}"),
    }
}
