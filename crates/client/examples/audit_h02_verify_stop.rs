//! Disposable Windows lab: keep a service VerifyImage RPC active while SYSTEM
//! asks SCM to stop the service. The watcher listens on localhost.
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use tokio::io::AsyncWriteExt;
    use tokio_stream::StreamExt;

    let image = std::env::args().nth(1).ok_or("image path missing")?;
    let port: u16 = std::env::args()
        .nth(2)
        .ok_or("watcher port missing")?
        .parse()?;
    let config = own_backup_common::config::AppConfig::default();
    let token = std::fs::read_to_string(config.token_path())?;
    let client =
        own_backup_client::BackupClient::connect(config.grpc_address, token.trim()).await?;
    let mut listener = client.clone();
    let events = listener.stream_events().await?;
    tokio::pin!(events);
    let mut verifier = client.clone();
    let task = tokio::spawn(async move {
        verifier
            .verify_image_with_status(&image, &own_backup_client::ArchiveSecret::none())
            .await
    });
    let active = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while let Some(event) = events.next().await {
            if let own_backup_client::ClientEvent::Unknown { kind, .. } = event {
                if kind == "verify" {
                    return true;
                }
            }
        }
        false
    })
    .await?;
    if !active || task.is_finished() {
        return Err("VerifyImage did not remain active".into());
    }
    println!("H02-VERIFY-ACTIVE progress_seen=true at={}", chrono_time());
    let mut watcher = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
    watcher.write_all(b"VERIFY\n").await?;
    watcher.shutdown().await?;
    println!("H02-VERIFY-STOP-SENT at={}", chrono_time());
    let result = tokio::time::timeout(std::time::Duration::from_secs(300), task).await??;
    match result {
        Ok(report) => {
            println!(
                "H02-VERIFY-RESULT success=true blocks={} mismatched={}",
                report.blocks_checked,
                report.mismatched_blocks.len()
            );
            Err("verification finished before the service stopped".into())
        }
        Err(error) => {
            println!(
                "H02-VERIFY-RESULT success=false code={:?} at={} error={error}",
                error.code(),
                chrono_time()
            );
            if error.code() == tonic::Code::Cancelled {
                Ok(())
            } else {
                Err(format!("verify did not report cancellation: {error}").into())
            }
        }
    }
}

fn chrono_time() -> String {
    // UTC milliseconds, independent of the guest's current locale.
    let since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    since_epoch.as_millis().to_string()
}
