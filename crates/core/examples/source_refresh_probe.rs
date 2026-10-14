//! Read-only check of `source_refresh` against the real disks: loads a
//! `jobs.json`, matches every job against the live inventory and prints what
//! a run would do — and whether each resulting device really sits in its GPT
//! slot, the check the engine makes before reading it. Writes nothing.
//!
//! `source_refresh_probe C:\ProgramData\OwnBackup\jobs.json`

#[cfg(target_os = "windows")]
#[tokio::main]
async fn main() {
    use own_backup_core::source_refresh::{reconcile_with_live_disks, Refreshed};

    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| r"C:\ProgramData\OwnBackup\jobs.json".into());
    let text = std::fs::read_to_string(&path).expect("jobs.json readable");
    let jobs: Vec<own_backup_core::Job> = serde_json::from_str(&text).expect("jobs.json parses");
    for job in jobs {
        println!("== {} ({})", job.name, job.id);
        println!("   sources now: {:?}", job.source_list());
        let job = match reconcile_with_live_disks(&job).await {
            Refreshed::Unchanged => {
                println!("   unchanged");
                job
            }
            Refreshed::Missing(why) => {
                println!("   MISSING: {why}");
                continue;
            }
            Refreshed::Updated(r) => {
                println!("   UPDATED: sources {:?}", r.job.source_list());
                for (old, new) in &r.renamed {
                    println!("   renamed {old} -> {new}");
                }
                println!("   note: {}", r.layout_note);
                r.job
            }
        };
        let Some(ds) = &job.disk_source else { continue };
        // Slots from the (refreshed) snapshot: the same IOCTL layout the
        // engine's GPT read agrees with, and readable without elevation.
        let Some(disk) = job.source_disks.iter().find(|d| {
            d.device_id
                .trim_start_matches(r"\\.\PhysicalDrive")
                .parse::<u32>()
                .ok()
                == Some(ds.disk_number)
        }) else {
            println!("   no snapshot for disk {}", ds.disk_number);
            continue;
        };
        for p in &ds.parts {
            let want = disk
                .partitions
                .iter()
                .find(|e| e.index == p.part_index)
                .map(|e| e.offset_bytes);
            let have = own_backup_backup::source_identity::extents_of_source(&p.device);
            let ok = matches!((&have, want), (Some(v), Some(off))
                if v.len() == 1 && v[0].disk == ds.disk_number && v[0].offset == off);
            println!(
                "   part {} {} slot@{:?} device@{:?} -> {}",
                p.part_index,
                p.device,
                want,
                have.map(|v| v.iter().map(|e| (e.disk, e.offset)).collect::<Vec<_>>()),
                if ok { "in slot" } else { "NOT IN SLOT" }
            );
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("Windows only");
}
