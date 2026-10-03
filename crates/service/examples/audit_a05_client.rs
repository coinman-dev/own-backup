//! Run inside a disposable Windows console, RunAs or actual RDP session.
//! Writes principal/session facts and the result of an authenticated read-only RPC.
#[cfg(windows)]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use own_backup_client::BackupClient;
    use own_backup_common::config::AppConfig;
    use windows::Win32::System::RemoteDesktop::{
        ProcessIdToSessionId, WTSGetActiveConsoleSessionId,
    };

    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 || !matches!(args[1].as_str(), "allow" | "deny") {
        return Err("usage: audit_a05_client.exe allow|deny <result.json>".into());
    }
    let config = AppConfig::default();
    let mut session = u32::MAX;
    unsafe {
        ProcessIdToSessionId(std::process::id(), &mut session)?;
    }
    let active = unsafe { WTSGetActiveConsoleSessionId() };
    let identity = std::process::Command::new("whoami.exe")
        .args(["/user", "/fo", "csv", "/nh"])
        .output()?;
    let identity = String::from_utf8_lossy(&identity.stdout).trim().to_string();
    let token = std::fs::read_to_string(config.token_path())?;
    if token.trim().len() != 64 {
        return Err("service token length differs from 64".into());
    }
    let rpc = tokio::time::timeout(std::time::Duration::from_secs(8), async {
        let mut client = BackupClient::connect(config.grpc_address, token.trim()).await?;
        // Read a cached DTO: GetServiceStatus runs DiskPart on Windows and
        // its cold-start latency would turn this access test into an I/O test.
        Ok::<_, Box<dyn std::error::Error>>(client.list_jobs().await?.len())
    })
    .await;
    let (allowed, error, timeout, jobs) = match rpc {
        Ok(Ok(count)) => (true, String::new(), false, Some(count)),
        Ok(Err(error)) => (false, error.to_string(), false, None),
        Err(error) => (false, error.to_string(), true, None),
    };
    let pass = !timeout && allowed == (args[1] == "allow");
    let report = serde_json::json!({"pid":std::process::id(), "session":session,
        "active_console":active, "identity":identity, "token_read":true, "token_bytes":64,
        "rpc_method":"ListJobs", "jobs_count":jobs, "rpc_allowed":allowed, "rpc_error":error,
        "rpc_timeout":timeout, "expected":args[1], "pass":pass});
    std::fs::write(&args[2], serde_json::to_vec_pretty(&report)?)?;
    println!("A05-CLIENT {report}");
    if !pass {
        return Err("A05 access result differs from policy".into());
    }
    Ok(())
}

#[cfg(not(windows))]
fn main() {
    eprintln!("audit_a05_client is Windows-only");
}
