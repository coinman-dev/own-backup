//! Dump the sidecar metadata of a backup image: embedded (in-image) first,
//! then the adjacent .obkm. Usage: sdump <image.{vhdx,avhdx,qcow2}>
fn main() {
    let path = std::path::PathBuf::from(std::env::args().nth(1).expect("usage: sdump <image>"));
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_lowercase();
    match own_backup_backup::read_embedded_sidecar_header(&path, &ext) {
        Ok((info, m)) => {
            println!("embedded: OK");
            println!("  format_version={} container={} role={:?}", info.format_version, info.container, info.chain_role);
            println!("  image_file_name={:?} parent={:?}", info.image_file_name, info.parent_image_file_name);
            println!("  job_id={:?} volume_data_offset={}", info.job_id, info.volume_data_offset);
            println!("  backup_id={} created_at={}", m.backup_id, m.created_at);
        }
        Err(e) => println!("embedded: ERR {e}"),
    }
    let sc = own_backup_backup::sidecar_path_for(&path);
    println!("external .obkm exists: {}", sc.exists());
}
