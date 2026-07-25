//! One-off: dump the embedded sidecar of an image (temporary dev tool).
fn main() {
    let path = std::env::args().nth(1).expect("usage: inspect_sidecar <image>");
    let sc = own_backup_backup::sidecar::read_image_sidecar_header(std::path::Path::new(&path));
    match sc {
        Err(e) => println!("ERROR reading sidecar: {e}"),
        Ok((info, manifest)) => {
            println!("container={} chain_role={:?} job_id={:?}", info.container, info.chain_role, info.job_id);
            println!("created_at={}", manifest.created_at);
            println!("source_disk.device_id={} size={} ({:.2} GiB)", manifest.source_disk.device_id, manifest.source_disk.size_bytes, manifest.source_disk.size_bytes as f64/1024f64.powi(3));
            println!("used_blocks={} total_blocks={} block_size={}", manifest.used_blocks, manifest.total_blocks, manifest.block_size);
            println!("excluded_files={} excluded_bytes={}", info.excluded_files, info.excluded_bytes);
            println!("categories={:?}", info.categories);
            match &info.source_disk_layout {
                None => println!("source_disk_layout=NONE"),
                Some(d) => {
                    println!("source_disk_layout: model={:?} size={} ({:.2} GiB) table={:?} parts={}", d.model, d.size_bytes, d.size_bytes as f64/1024f64.powi(3), d.partition_table, d.partitions.len());
                    for p in &d.partitions {
                        println!("  part idx={} off={} ({:.2} MiB) size={} ({:.2} GiB) fs={:?} letter={:?} label={:?} used={} boot={} sys={} enc={}",
                            p.index, p.offset_bytes, p.offset_bytes as f64/1024f64.powi(2), p.size_bytes, p.size_bytes as f64/1024f64.powi(3), p.filesystem, p.volume_letter, p.volume_label, p.used_bytes, p.is_boot, p.is_system, p.is_encrypted);
                    }
                }
            }
        }
    }
    // Whole-disk manifest, if any.
    match own_backup_backup::sidecar::read_image_sidecar(std::path::Path::new(&path)) {
        Ok(sc) => match sc.disk_manifest {
            None => println!("disk_manifest=NONE"),
            Some(dm) => {
                println!("disk_manifest: disk_number={} model={:?} layout.size={} ({:.2} GiB) sector={} gpt_parts={} images={}",
                    dm.disk_number, dm.disk_model, dm.layout.size_bytes, dm.layout.size_bytes as f64/1024f64.powi(3), dm.layout.sector_size, dm.layout.partitions.len(), dm.images.len());
                for p in &dm.layout.partitions {
                    let sz = (p.last_lba - p.first_lba + 1) * dm.layout.sector_size as u64;
                    println!("  gpt idx={} first_lba={} last_lba={} size={} ({:.2} GiB) name={:?}", p.index, p.first_lba, p.last_lba, sz, sz as f64/1024f64.powi(3), p.name);
                }
                for i in &dm.images { println!("  image part_index={} token={} file={:?} fs={:?}", i.part_index, i.token, i.image_file_name, i.filesystem); }
                println!("dm.source_disk={}", if dm.source_disk.is_some() {"SOME"} else {"NONE"});
            }
        },
        Err(e) => println!("full sidecar read error: {e}"),
    }
}
