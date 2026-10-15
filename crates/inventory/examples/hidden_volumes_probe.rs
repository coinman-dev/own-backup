//! Read-only: which partitions of this machine the «Буквы скрытых разделов»
//! card would list. Needs no elevation and changes nothing.

#[cfg(target_os = "windows")]
#[tokio::main]
async fn main() {
    use own_backup_inventory::DiskInventory;
    let disks = own_backup_inventory::WindowsInventory::new()
        .unwrap()
        .enumerate_disks()
        .await
        .unwrap();
    for d in &disks {
        for p in &d.partitions {
            println!(
                "{} #{} {:?} type={} attrs={:#018x} mbr={:#04x} dev={:?} hidden={}",
                d.device_id,
                p.index,
                p.filesystem,
                p.gpt_type_guid,
                p.gpt_attributes,
                p.mbr_type,
                p.volume_device,
                p.is_hidden_system(d.partition_table)
            );
        }
    }
    println!("--- listed:");
    for v in own_backup_common::disk::hidden_volumes(&disks) {
        println!("{v:?}");
    }
}

#[cfg(not(target_os = "windows"))]
fn main() {}
