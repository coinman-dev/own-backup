//! Exercise stale archive-encryption confirmations against the live service.
#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().expect("mode: edited|deleted|queued|plaintext");
    let id = args.next().expect("job id");
    let token =
        std::fs::read_to_string(own_backup_common::config::AppConfig::default().token_path())
            .unwrap_or_default();
    let mut client =
        own_backup_client::BackupClient::connect("http://127.0.0.1:50051", token.trim())
            .await
            .expect("connect to service");
    let job = client
        .get_job(&id)
        .await
        .expect("get job")
        .expect("job exists");
    let begun = client
        .begin_archive_encryption(&job, "AuditOnly!2026")
        .await
        .expect("begin encryption");
    println!("begun mode={mode} job={id}");
    match mode.as_str() {
        "edited" => {
            let mut changed = client.get_job(&id).await.unwrap().unwrap();
            changed.name.push_str("-changed");
            client.save_job(&changed).await.expect("edit job");
        }
        "deleted" => {
            client.delete_job(&id, false).await.expect("delete job");
        }
        "queued" => {
            client.run_job(&id).await.expect("queue job");
            let q = client.get_queue().await.expect("get queue");
            let in_queue =
                q.current.as_deref() == Some(id.as_str()) || q.pending.iter().any(|j| j == &id);
            println!("queued_before_confirmation={in_queue}");
            assert!(in_queue, "job finished before queued confirmation test");
        }
        "plaintext" => {
            client.run_job(&id).await.expect("create plaintext copy");
            let mut success = false;
            for _ in 0..200 {
                let current = client.get_job(&id).await.unwrap().unwrap();
                if let Some(run) = current.last_run {
                    if run.status == "success" {
                        success = true;
                        break;
                    }
                    if run.status == "error" {
                        panic!("plaintext backup failed: {}", run.error);
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            println!("plaintext_copy_created={success}");
            assert!(success, "plaintext backup did not complete");
            let mut queue_cleared = false;
            for _ in 0..100 {
                let q = client
                    .get_queue()
                    .await
                    .expect("get queue after plaintext backup");
                if q.current.as_deref() != Some(id.as_str()) && !q.pending.iter().any(|j| j == &id)
                {
                    queue_cleared = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            println!("queue_cleared_before_confirmation={queue_cleared}");
            assert!(queue_cleared, "plaintext backup still held the queue");
        }
        _ => panic!("unknown mode {mode}"),
    }

    let rejected = client
        .confirm_archive_encryption(&begun.token, &begun.recovery_key)
        .await
        .expect_err("stale confirmation must be rejected");
    println!("confirmation_rejected mode={mode} reason={rejected}");
    if mode == "queued" {
        client
            .cancel_running()
            .await
            .expect("cancel queued or running backup");
    }
    let after = client.get_job(&id).await.expect("get job after rejection");
    if mode == "deleted" {
        assert!(after.is_none(), "deleted job was resurrected");
    } else {
        let after = after.expect("job disappeared");
        assert!(
            after.archive_encryption.is_none(),
            "stale confirmation changed authorization"
        );
    }
    println!("plaintext_state_preserved mode={mode}");
}
