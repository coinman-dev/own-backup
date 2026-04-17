use clap::Args;
use own_backup_inventory::PreflightAnalyzer;

#[derive(Args)]
pub struct PreflightArgs {
    /// Device to analyze (e.g., C: or \\.\PhysicalDrive0)
    pub device: String,

    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

pub async fn run(args: PreflightArgs) -> own_backup_common::Result<()> {
    let analyzer = PreflightAnalyzer::new();
    let report = analyzer.analyze(&args.device).map_err(|e| e)?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("Preflight Report for {}", args.device);
        println!("  Boot mode:      {}", report.boot_mode);
        println!("  Partition table: {:?}", report.partition_table);
        println!("  Disk type:       {:?}", report.disk_type);
        println!("  Filesystem:      {:?}", report.filesystem);
        println!("  Support tier:    {}", report.support_tier);
        println!(
            "  VSS running:     {}",
            if report.vss_service_running {
                "Yes"
            } else {
                "No"
            }
        );
        println!(
            "  VSS writers:     {}",
            if report.vss_writers_healthy {
                "Healthy"
            } else {
                "Issues detected"
            }
        );

        if !report.warnings.is_empty() {
            println!("\nWarnings:");
            for w in &report.warnings {
                println!("  [!] {}", w);
            }
        }
        if !report.blockers.is_empty() {
            println!("\nBlockers:");
            for b in &report.blockers {
                println!("  [X] {}", b);
            }
        }
    }

    Ok(())
}
