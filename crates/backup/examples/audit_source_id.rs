//! Print source identity from an image's embedded or external sidecar.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    for path in std::env::args().skip(1) {
        let (info, _) =
            own_backup_backup::sidecar::read_image_sidecar_header(std::path::Path::new(&path))?;
        println!(
            "{} source_id={:?} role={:?} parent={:?} job_id={:?}",
            path, info.source_id, info.chain_role, info.parent_image_file_name, info.job_id
        );
    }
    Ok(())
}
