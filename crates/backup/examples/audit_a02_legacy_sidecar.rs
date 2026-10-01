//! Lab-only conversion of real disposable qcow2 images to the old .obkm form.
use own_backup_backup::qcow2::merge::write_qcow2_guest_blob;
use own_backup_backup::sidecar::{
    meta_partition_of, read_embedded_sidecar, read_embedded_sidecar_header, read_image_sidecar,
    sidecar_path_for, write_sidecar,
};
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let paths: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
    if paths.len() != 2 {
        return Err("exactly one disposable full and increment image required".into());
    }
    for path in paths {
        if !path
            .to_string_lossy()
            .to_ascii_lowercase()
            .starts_with(r"c:\lab\a02-2026-10-01-images\")
            || path.extension().and_then(|ext| ext.to_str()) != Some("qcow2")
        {
            return Err(format!("outside the disposable A02 fixture: {}", path.display()).into());
        }
        let sidecar = read_embedded_sidecar(&path, "qcow2")?;
        if sidecar.disk_manifest.is_some() {
            return Err("whole-disk metadata must not be converted to old sidecar".into());
        }
        let external = sidecar_path_for(&path);
        if external.exists() {
            return Err(format!("external sidecar already exists: {}", external.display()).into());
        }
        let (offset, _) = meta_partition_of(&path, "qcow2")?;
        write_sidecar(
            &external,
            &sidecar.info,
            &sidecar.manifest,
            &sidecar.checksums,
        )?;
        write_qcow2_guest_blob(&path, offset, &[0u8; 12])?;
        if read_embedded_sidecar_header(&path, "qcow2").is_ok() {
            return Err("embedded sidecar remains readable".into());
        }
        let fallback = read_image_sidecar(&path)?;
        if fallback.info.job_id != sidecar.info.job_id
            || fallback.checksums.len() != sidecar.checksums.len()
        {
            return Err("external sidecar differs from the embedded original".into());
        }
        println!(
            "A02-LEGACY image={} sidecar={} sidecar_bytes={} checksums={} fallback=true",
            path.display(),
            external.display(),
            std::fs::metadata(&external)?.len(),
            fallback.checksums.len()
        );
    }
    Ok(())
}
