//! Inspect the complete live DeleteJob RPC result in the disposable Windows lab.
#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let id = args.next().expect("job id");
    let base = args.next().expect("base image path");
    let child = args.next().expect("child image path");
    let token =
        std::fs::read_to_string(own_backup_common::config::AppConfig::default().token_path())
            .unwrap_or_default();
    let mut client =
        own_backup_client::BackupClient::connect("http://127.0.0.1:50051", token.trim())
            .await
            .expect("connect to service");
    let report = client.delete_job(&id, true).await.expect("delete job");
    println!("{}", serde_json::to_string(&report).unwrap());
    let kept: std::collections::HashSet<&str> =
        report.kept.iter().map(|(path, _)| path.as_str()).collect();
    let disjoint = report
        .deleted
        .iter()
        .all(|path| !kept.contains(path.as_str()));
    let base_kept = kept.contains(base.as_str());
    let child_kept = kept.contains(child.as_str());
    println!(
        "disjoint={disjoint} base_kept={base_kept} child_kept={child_kept} deleted={}",
        report.deleted.len()
    );
    assert!(disjoint && base_kept && child_kept && report.deleted.is_empty());
}
