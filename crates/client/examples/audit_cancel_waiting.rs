//! Hold the service's busy slot with verification, then cancel a waiting job.
#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let image = args.next().expect("image path");
    let job_id = args.next().expect("job id");
    let token =
        std::fs::read_to_string(own_backup_common::config::AppConfig::default().token_path())
            .unwrap_or_default();
    let client = own_backup_client::BackupClient::connect("http://127.0.0.1:50051", token.trim())
        .await
        .expect("connect to service");
    let mut verifier = client.clone();
    let verify_image = image.clone();
    let verify = tokio::spawn(async move {
        verifier
            .verify_image(&verify_image, &own_backup_client::ArchiveSecret::none())
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        !verify.is_finished(),
        "verification finished before queueing backup"
    );

    let mut control = client.clone();
    control.run_job(&job_id).await.expect("queue backup");
    let mut waiting = false;
    for _ in 0..100 {
        let q = control.get_queue().await.expect("get queue");
        if q.current.as_deref() == Some(job_id.as_str()) && !verify.is_finished() {
            waiting = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    println!("verify_active_while_job_waited={waiting}");
    assert!(
        waiting,
        "backup was not observed waiting behind verification"
    );
    control
        .cancel_running()
        .await
        .expect("cancel waiting backup");
    println!("cancel_sent_while_verify_active={}", !verify.is_finished());
    assert!(
        !verify.is_finished(),
        "verification ended before cancellation"
    );
    let verify_result = verify.await.expect("verify task");
    println!("verify_result={verify_result:?}");

    for _ in 0..100 {
        let job = control
            .get_job(&job_id)
            .await
            .expect("get job")
            .expect("job exists");
        if let Some(run) = job.last_run {
            if !run.finished_at.is_empty() && run.status == "cancelled" {
                println!("backup_status=cancelled finished={}", run.finished_at);
                return;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("cancelled backup result not recorded");
}
