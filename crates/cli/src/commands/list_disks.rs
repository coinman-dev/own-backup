use clap::Args;
use own_backup_inventory::DiskInventory;

#[derive(Args)]
pub struct ListDisksArgs {
    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

pub async fn run(args: ListDisksArgs) -> own_backup_common::Result<()> {
    #[cfg(target_os = "windows")]
    let inventory = own_backup_inventory::WmiInventory::new()?;

    #[cfg(not(target_os = "windows"))]
    let inventory = own_backup_inventory::MockInventory::default();

    let disks = inventory.enumerate_disks().await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&disks)?);
    } else {
        for disk in &disks {
            println!(
                "Disk: {} | {} | {} | {:.1} GB",
                disk.device_id,
                disk.model,
                disk.media_type,
                disk.size_bytes as f64 / 1_073_741_824.0
            );
            for part in &disk.partitions {
                println!(
                    "  Partition {}: {:?} | {:.1} GB | {}{}",
                    part.index,
                    part.filesystem,
                    part.size_bytes as f64 / 1_073_741_824.0,
                    part.volume_letter
                        .map(|c| format!("{}:", c))
                        .unwrap_or_default(),
                    if part.is_system { " [EFI]" } else { "" },
                );
            }
        }
    }

    Ok(())
}
