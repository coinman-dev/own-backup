//! Inspect the complete live DeleteJob RPC result in the disposable Windows lab.
#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let id = args.next().expect("job id");
    let report_path = args.next().expect("report path");
    let protected: Vec<String> = args.collect();
    assert!(protected.len() >= 2, "protected image paths required");
    let token =
        std::fs::read_to_string(own_backup_common::config::AppConfig::default().token_path())
            .unwrap_or_default();
    let mut client =
        own_backup_client::BackupClient::connect("http://127.0.0.1:50051", token.trim())
            .await
            .expect("connect to service");
    let report = client.delete_job(&id, true).await.expect("delete job");
    let json = serde_json::to_string_pretty(&report).unwrap();
    std::fs::write(report_path, &json).expect("save complete DeleteJob reply");
    println!("{json}");
    let kept: std::collections::HashSet<&str> =
        report.kept.iter().map(|(path, _)| path.as_str()).collect();
    let disjoint = report
        .deleted
        .iter()
        .all(|path| !kept.contains(path.as_str()));
    let protected_kept = protected
        .iter()
        .filter(|path| kept.contains(path.as_str()))
        .count();
    println!(
        "disjoint={disjoint} protected_kept={protected_kept}/{} deleted={}",
        protected.len(),
        report.deleted.len()
    );
    assert!(disjoint && protected_kept == protected.len() && report.deleted.is_empty());
}
