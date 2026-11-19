//! Prints how each job's next run would be outlined for the round indicator,
//! from the copies on its backup disk. Reads only.
//!
//! `run_outline_probe C:\ProgramData\OwnBackup\jobs.json`

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| r"C:\ProgramData\OwnBackup\jobs.json".into());
    let text = std::fs::read_to_string(&path).expect("jobs.json readable");
    let jobs: Vec<own_backup_core::Job> = serde_json::from_str(&text).expect("jobs.json parses");
    for job in jobs {
        println!("== {} -> {}", job.name, job.dest_folder);
        println!("   {}", own_backup_core::run_outline_for_probe(&job));
    }
}
