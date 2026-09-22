//! Tauri commands for restore / verify / mount used by the Restore & Tools views.
//!
//! These operations need admin (raw device I/O, VHDX attach), so they run in the
//! privileged service and the (unelevated) GUI proxies them over gRPC. Progress
//! arrives on the service event stream and is re-emitted by `bridge_service_events`
//! as `restore-progress` / `verify-progress` / `restore-disk-progress`.
//!
//! `read_image_info` and `list_disk_backups` are pure file reads (no privileges)
//! and stay in-process, cross-platform.

use serde::Serialize;
use tauri::State;

use own_backup_backup::exclusions::CategorySizes;
use own_backup_common::disk::DiskInfo;
use own_backup_common::engine_dto::{DiskRestoreSummaryDto, RestoreSummaryDto, VerifySummaryDto};
use own_backup_common::restore_check::CheckContext;
use own_backup_common::restore_plan::RestorePlan;
use own_backup_common::restore_state::RestoreRunState;

use crate::state::GuiState;

/// One partition of a whole-disk (`.obdm`) backup, for the restore list.
#[derive(Debug, Clone, Serialize)]
pub struct DiskBackupPartDto {
    pub index: u32,
    pub name: String,
    pub filesystem: String,
    pub size_bytes: u64,
}

/// The filesystem the image records for one partition.
#[derive(Debug, Clone, Serialize)]
pub struct PartFsDto {
    pub index: u32,
    pub filesystem: String,
}

/// One encrypted partition's container, as this copy took it.
#[derive(Debug, Clone, Serialize)]
pub struct PartEncryptionDto {
    pub index: u32,
    /// `bitlocker` | `luks1` | `luks2` | `other`.
    pub kind: String,
    /// `plaintext` — the copy holds the volume decrypted, and a restore
    /// produces an unencrypted volume; `sector` — the container's own bytes
    /// went in whole, and a restore brings the encryption back with them.
    pub captured: String,
}

/// A whole-disk backup discovered in the destination folder (from a `.obdm`).
#[derive(Debug, Clone, Serialize)]
pub struct DiskBackupDto {
    pub manifest_path: String,
    pub disk_model: String,
    pub disk_number: u32,
    pub created_at: String,
    pub total_bytes: u64,
    pub partitions: Vec<DiskBackupPartDto>,
}

/// List whole-disk backups found in `folder`: single whole-disk IMAGES that
/// carry an embedded manifest (plan D1), and legacy `.obdm` manifests. The
/// returned `manifest_path` is what to hand to restore (the image itself for an
/// embedded backup, the `.obdm` for a legacy one). Cross-platform read-only.
#[tauri::command]
pub async fn list_disk_backups(folder: String) -> Result<Vec<DiskBackupDto>, String> {
    use own_backup_common::gpt::DiskBackupManifest;

    let dir = std::path::Path::new(&folder);
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(Vec::new()), // missing folder → no disk backups
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_lowercase();
        // Whole-disk image (embedded manifest) OR legacy .obdm; skip the rest.
        let m: DiskBackupManifest = if ext == DiskBackupManifest::EXTENSION {
            let Ok(raw) = std::fs::read(&path) else { continue };
            let Ok(m) = serde_json::from_slice::<DiskBackupManifest>(&raw) else {
                continue;
            };
            m
        } else if matches!(ext.as_str(), "vhdx" | "avhdx" | "qcow2") {
            match own_backup_backup::read_embedded_sidecar(&path, &ext) {
                Ok(sc) => match sc.disk_manifest {
                    Some(m) => m,           // a whole-disk image
                    None => continue,       // a plain per-volume image
                },
                Err(_) => continue,
            }
        } else {
            continue;
        };
        let sector = m.layout.sector_size;
        let partitions: Vec<DiskBackupPartDto> = m
            .images
            .iter()
            .filter_map(|img| {
                m.layout
                    .partitions
                    .iter()
                    .find(|p| p.index == img.part_index)
                    .map(|p| DiskBackupPartDto {
                        index: p.index,
                        name: p.name.clone(),
                        filesystem: img.filesystem.clone(),
                        size_bytes: p.size_bytes(sector),
                    })
            })
            .collect();
        let total_bytes = partitions.iter().map(|p| p.size_bytes).sum();
        out.push(DiskBackupDto {
            manifest_path: path.display().to_string(),
            disk_model: m.disk_model,
            disk_number: m.disk_number,
            created_at: m.created_at,
            total_bytes,
            partitions,
        });
    }
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(out)
}

/// Metadata about an image, read from its `.obkm` sidecar (works on any OS).
#[derive(Debug, Clone, Serialize)]
pub struct ImageInfoDto {
    /// Full path to the image file (what restore is handed).
    pub image_path: String,
    /// Job id that produced this image (ties copies of one archive together).
    pub job_id: Option<String>,
    pub image_file_name: String,
    pub chain_role: String,
    pub created_at: String,
    pub source_device: String,
    pub source_bytes: u64,
    pub block_size: u32,
    pub used_blocks: u64,
    pub total_blocks: u64,
    pub backup_mode: String,
    /// Full physical-disk snapshot embedded at backup time (whole disk with its
    /// partitions), so the restore card renders from the image alone — no
    /// jobs.json. `None` for images written before snapshots existed.
    pub source_disk_layout: Option<DiskInfo>,
    /// Partition indices (into `source_disk_layout.partitions`) that this backup
    /// actually contains — the ones the restore card highlights.
    pub backed_up_parts: Vec<u32>,
    /// Filesystem inside each backed-up partition, **as the image records it**.
    ///
    /// For a LUKS or BitLocker source this is the decrypted filesystem, because
    /// that is what the image holds. The disk snapshot alongside says `Unknown`
    /// and flags the partition encrypted, which is the only honest reading of a
    /// container nobody has opened — but for a partition that is *in the copy*
    /// the manifest knows better, and a padlock over what restores as plain
    /// btrfs would be a lie (D14).
    pub part_filesystems: Vec<PartFsDto>,
    /// Partitions whose encrypted container's header this copy kept, so a
    /// restore can put the encryption back exactly as it was (D19). Empty for
    /// copies taken before headers were stored — those restore decrypted only.
    pub luks_header_parts: Vec<u32>,
    /// What each encrypted partition's container was, and how this copy took
    /// it. The restore screen says opposite things for BitLocker and LUKS, and
    /// opposite things again for a container copied in the clear and one copied
    /// verbatim — so it needs the fact, not an inference from the filesystem.
    /// Empty for copies written before this was recorded.
    pub encrypted_parts: Vec<PartEncryptionDto>,
    /// File-type breakdown embedded in the image (for "по типам файлов").
    pub categories: Option<CategorySizes>,
    /// Files/bytes excluded from this image (for the "Исключено" legend line).
    pub excluded_files: u64,
    pub excluded_bytes: u64,
    /// Logical sector size the source disk's GPT was captured with. A restore
    /// onto a disk with a different sector size cannot reproduce the layout:
    /// the stored LBAs would address different byte offsets.
    pub sector_size: u32,
    /// True when this image is a whole-disk (bootable) backup.
    pub is_whole_disk: bool,
    /// Version of Own Backup that wrote this copy. Empty for copies made before
    /// the field existed — worth showing as such rather than hiding, since
    /// "which build made this" is the first question when a copy misbehaves.
    pub app_version: String,
    /// The machine's UEFI boot entries this copy carries, if any.
    ///
    /// Present only for a copy taken with "Загрузочная копия диска" on. It is
    /// here so the restore screen can show the user the actual menu it is
    /// offering to put back — a list of names is the only way "restore the boot
    /// entries" means anything to the person answering the question.
    pub boot_entries: Option<BootEntriesDto>,
}

/// What a copy holds of the machine's boot menu.
#[derive(Debug, Clone, Serialize)]
pub struct BootEntriesDto {
    /// Which machine it came off. The entries name partitions by GUID, so a
    /// copy taken elsewhere restores menu items that point at nothing here.
    pub machine: String,
    /// The machine this is being read ON, so the screen can say whether the
    /// two are the same without the frontend having to find out what this
    /// computer is called. Empty when the name is not available.
    pub this_machine: String,
    pub taken_at: String,
    /// How many firmware variables would actually be written.
    pub variables: usize,
    /// The readable half: what those bytes mean, for the user to look at.
    pub entries: Vec<BootEntryDto>,
}

/// One line of that menu.
#[derive(Debug, Clone, Serialize)]
pub struct BootEntryDto {
    /// `Boot0000`, `BootOrder`, …
    pub name: String,
    /// "Windows 11", "Kubuntu 24.04" — what the firmware shows.
    pub description: String,
    /// The loader it starts, when the entry names one.
    pub file: Option<String>,
}

/// Read one image's embedded metadata into an [`ImageInfoDto`] (direct file
/// read: embedded meta partition first, then an adjacent `.obkm`). Shared by
/// `read_image_info` (single file) and `list_image_backups` (folder scan).
/// Load an image's sidecar: embedded first, the adjacent `.obkm` as a fallback
/// for images written before the sidecar moved inside the container.
/// The keys for an encrypted copy, from what the person typed. Never the
/// service's own (§7.4) — this runs in the unelevated interface, which has no
/// access to that store at all.
fn archive_keys(
    path: &std::path::Path,
    secret: &own_backup_client::ArchiveSecret,
) -> Result<own_backup_backup::seal::context::ArchiveKeys, String> {
    use own_backup_backup::seal::context::ArchiveKeys;
    use own_backup_backup::seal::recovery::RecoveryKey;

    if let Some(password) = secret.password.as_ref().filter(|p| !p.is_empty()) {
        return ArchiveKeys::from_image_password(path, password).map_err(|e| e.to_string());
    }
    if let Some(typed) = secret.recovery_key.as_ref().filter(|k| !k.trim().is_empty()) {
        let key = RecoveryKey::parse(typed).map_err(|e| e.to_string())?;
        return ArchiveKeys::from_image_recovery(path, &key).map_err(|e| e.to_string());
    }
    Err(format!(
        "{}: копия зашифрована — нужен пароль или ключ восстановления",
        path.display()
    ))
}

fn load_sidecar(
    path: &std::path::Path,
    secret: &own_backup_client::ArchiveSecret,
) -> Result<own_backup_backup::Sidecar, String> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_lowercase();
    // An encrypted copy keeps everything this screen shows — the disk layout,
    // the partitions, the sizes — inside the sealed half. Without the key there
    // is nothing here to read, so the key comes in with the request.
    if ext == own_backup_backup::seal::obk::OBK_EXTENSION {
        let keys = archive_keys(path, secret)?;
        let chain = keys.open_chain_of(path).map_err(|e| e.to_string())?;
        return own_backup_backup::sidecar::read_sealed_sidecar(path, &chain.key)
            .map_err(|e| e.to_string());
    }
    match own_backup_backup::read_embedded_sidecar(path, &ext) {
        Ok(s) => Ok(s),
        Err(_) => {
            let sidecar_path = own_backup_backup::sidecar_path_for(path);
            if !sidecar_path.exists() {
                return Err(format!(
                    "В образе нет встроенных метаданных и рядом не найден файл метаданных: {}",
                    sidecar_path.display()
                ));
            }
            own_backup_backup::read_sidecar(&sidecar_path).map_err(|e| e.to_string())
        }
    }
}

fn read_image_info_at(
    path: &std::path::Path,
    secret: &own_backup_client::ArchiveSecret,
) -> Result<ImageInfoDto, String> {
    let sidecar = load_sidecar(path, secret)?;

    // Derived (compute before moving fields out of the sidecar): the archived
    // disk snapshot and which of its partitions are in this backup — whole-disk
    // images list them in the embedded disk manifest; a per-volume image holds
    // the single partition matching its source device.
    let source_disk_layout = sidecar.info.source_disk_layout.clone();
    let is_whole_disk = sidecar.disk_manifest.is_some();
    let src_dev = sidecar.manifest.source_disk.device_id.clone();
    let backed_up_parts: Vec<u32> = if let Some(dm) = &sidecar.disk_manifest {
        dm.images.iter().map(|i| i.part_index).collect()
    } else if let Some(layout) = &source_disk_layout {
        layout
            .partitions
            .iter()
            .filter(|p| p.volume_device == src_dev)
            .map(|p| p.index)
            .collect()
    } else {
        Vec::new()
    };

    // Whole-disk images carry a filesystem per partition in the disk manifest;
    // a per-volume image carries the one filesystem it holds.
    let part_filesystems: Vec<PartFsDto> = if let Some(dm) = &sidecar.disk_manifest {
        dm.images
            .iter()
            .filter(|i| !i.filesystem.is_empty())
            .map(|i| PartFsDto { index: i.part_index, filesystem: i.filesystem.clone() })
            .collect()
    } else {
        match (&sidecar.info.filesystem, backed_up_parts.first()) {
            (Some(fs), Some(&index)) if !fs.is_empty() => {
                vec![PartFsDto { index, filesystem: fs.clone() }]
            }
            _ => Vec::new(),
        }
    };

    // A per-volume image records index 0 (there was nothing to disambiguate at
    // backup time), so its lone header answers for the partition it holds.
    let luks_header_parts: Vec<u32> = if sidecar.disk_manifest.is_none()
        && sidecar.info.luks_headers.len() == 1
    {
        backed_up_parts.clone()
    } else {
        sidecar.info.luks_headers.iter().map(|h| h.part_index).collect()
    };

    // What each encrypted partition was. A whole-disk image says it per
    // partition in the disk manifest; a per-volume image says it once, about
    // the one volume it holds.
    let enc_name = |k: &own_backup_common::image::VolumeEncryptionKind| {
        use own_backup_common::image::VolumeEncryptionKind as K;
        match k {
            K::BitLocker => "bitlocker",
            K::Luks1 => "luks1",
            K::Luks2 => "luks2",
            K::Other => "other",
        }
        .to_string()
    };
    let captured_name = |m: &own_backup_common::image::VolumeCaptureMode| {
        use own_backup_common::image::VolumeCaptureMode as M;
        match m {
            M::Plaintext => "plaintext",
            M::SectorBySector => "sector",
        }
        .to_string()
    };
    let encrypted_parts: Vec<PartEncryptionDto> = match &sidecar.disk_manifest {
        Some(dm) => dm
            .images
            .iter()
            .filter_map(|i| {
                i.encryption.as_ref().map(|e| PartEncryptionDto {
                    index: i.part_index,
                    kind: enc_name(&e.kind),
                    captured: captured_name(&e.capture_mode),
                })
            })
            .collect(),
        None => sidecar
            .manifest
            .volume_encryption
            .as_ref()
            .and_then(|e| {
                backed_up_parts.first().map(|&index| PartEncryptionDto {
                    index,
                    kind: enc_name(&e.kind),
                    captured: captured_name(&e.capture_mode),
                })
            })
            .into_iter()
            .collect(),
    };

    Ok(ImageInfoDto {
        image_path: path.display().to_string(),
        job_id: sidecar.info.job_id.clone(),
        image_file_name: sidecar.info.image_file_name,
        chain_role: format!("{:?}", sidecar.info.chain_role),
        created_at: sidecar.manifest.created_at,
        source_device: src_dev,
        source_bytes: sidecar.manifest.source_disk.size_bytes,
        block_size: sidecar.manifest.block_size,
        used_blocks: sidecar.manifest.used_blocks,
        total_blocks: sidecar.manifest.total_blocks,
        backup_mode: format!("{:?}", sidecar.manifest.backup_mode),
        source_disk_layout,
        backed_up_parts,
        part_filesystems,
        luks_header_parts,
        encrypted_parts,
        categories: sidecar.info.categories,
        excluded_files: sidecar.info.excluded_files,
        excluded_bytes: sidecar.info.excluded_bytes,
        // The GPT's own sector size is authoritative for a whole-disk image;
        // otherwise fall back to the volume's.
        sector_size: sidecar
            .disk_manifest
            .as_ref()
            .map(|dm| dm.layout.sector_size)
            .unwrap_or(sidecar.manifest.source_disk.sector_size_logical),
        is_whole_disk,
        app_version: sidecar.info.app_version,
        // Inside the image now; a copy taken before they moved in still has
        // them in a `.boot.json` beside it, and that is read as a fallback.
        boot_entries: own_backup_backup::sidecar::boot_entries_of_image(path).map(|b| {
            // A copy taken before flash drives were left out still carries
            // them, and the restore no longer writes them back — so they have
            // no business being in the list the user is asked to agree to.
            let removable: std::collections::HashSet<String> = b
                .variables
                .iter()
                .filter(|v| own_backup_backup::efivars::names_removable_device(v))
                .map(|v| v.name.clone())
                .collect();
            BootEntriesDto {
                machine: b.machine.clone(),
                this_machine: std::env::var("COMPUTERNAME")
                    .or_else(|_| std::env::var("HOSTNAME"))
                    .unwrap_or_default(),
                taken_at: b.taken_at.clone(),
                variables: b.variables.len() - removable.len(),
                entries: b
                    .readable
                    .iter()
                    // `BootOrder` and `Timeout` are settings, not menu items;
                    // listing them among the systems would be answering a
                    // different question than the one on screen.
                    .filter(|n| !n.description.trim().is_empty())
                    .filter(|n| !removable.contains(&n.name))
                    .map(|n| BootEntryDto {
                        name: n.name.clone(),
                        description: n.description.clone(),
                        file: n.file.clone(),
                    })
                    .collect(),
            }
        }),
    })
}

#[tauri::command]
pub async fn read_image_info(
    image_path: String,
    password: Option<String>,
    recovery_key: Option<String>,
) -> Result<ImageInfoDto, String> {
    read_image_info_at(
        &std::path::PathBuf::from(&image_path),
        &secret_of(password, recovery_key),
    )
}

/// Scan `folder` for backup IMAGES (per-volume AND whole-disk) and return each
/// one's embedded metadata. Direct file reads, no service / no jobs.json — the
/// "Открыть папку с копиями" entry for a bare-system (Boot-CD) restore. Files
/// without a readable sidecar are skipped; newest first.
#[tauri::command]
pub async fn list_image_backups(folder: String) -> Result<Vec<ImageInfoDto>, String> {
    let dir = std::path::Path::new(&folder);
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(Vec::new()),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_lowercase();
        // `.obk` is absent here because everything this DTO holds — the layout,
        // the partitions, the sizes — is sealed, and a folder scan has no key.
        // They are not ignored: [`list_locked_images`] finds them by their open
        // header so the screen can ask for the password instead of reporting an
        // empty folder that plainly is not empty.
        if !matches!(ext.as_str(), "vhdx" | "avhdx" | "qcow2") {
            continue;
        }
        if let Ok(info) = read_image_info_at(&path, &own_backup_client::ArchiveSecret::none()) {
            out.push(info);
        }
    }
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(out)
}

/// One encrypted copy found in a folder, as far as it can be read with no key.
///
/// Every field comes from the open header — the same half the backup lists and
/// retention work from. It is a CLAIM: enough to name the file and group it
/// with its chain, and nothing a restore may act on. The sealed metadata is
/// read once the person supplies the password.
#[derive(Debug, Clone, Serialize)]
pub struct LockedImageDto {
    pub image_path: String,
    pub file_name: String,
    pub job_id: String,
    pub chain_id: String,
    pub backup_id: String,
    pub role: String,
    pub created_at: String,
    /// The file this one continues, when it continues one. A copy whose parent
    /// is missing from the folder cannot be restored from it.
    pub parent_file_name: Option<String>,
    pub virtual_size: u64,
}

/// Scan `folder` for ENCRYPTED copies, without any key.
///
/// The counterpart of [`list_image_backups`], which can only describe copies it
/// can read. Without this a folder holding nothing but `.obk` files answered
/// "no copies with metadata found" — true of the DTO, and entirely wrong about
/// the folder, which is the one thing the person could see for themselves.
#[tauri::command]
pub async fn list_locked_images(folder: String) -> Result<Vec<LockedImageDto>, String> {
    let dir = std::path::Path::new(&folder);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_lowercase();
        if ext != own_backup_backup::seal::obk::OBK_EXTENSION {
            continue;
        }
        let Ok(listing) = own_backup_backup::seal::obk::read_listing(&path) else {
            continue;
        };
        out.push(LockedImageDto {
            image_path: path.to_string_lossy().into_owned(),
            file_name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            job_id: listing.job_id,
            chain_id: listing.chain_id.to_string(),
            backup_id: listing.backup_id.to_string(),
            role: format!("{:?}", listing.role).to_lowercase(),
            created_at: listing.created_at,
            parent_file_name: listing.parent_file_name,
            virtual_size: listing.virtual_size,
        });
    }
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(out)
}

// --- Proxies to the service's privileged engine --------------------------

/// What the screen collected to open an encrypted copy, if anything.
///
/// The interface asks [`inspect_backup_image`] first and prompts only when the
/// answer says the copy is encrypted; a manual operation never borrows the
/// service's own key (§7.4).
fn secret_of(
    password: Option<String>,
    recovery_key: Option<String>,
) -> own_backup_client::ArchiveSecret {
    own_backup_client::ArchiveSecret {
        password: password.filter(|p| !p.is_empty()),
        recovery_key: recovery_key.filter(|k| !k.trim().is_empty()),
    }
}

/// What a copy is, without any key — so the interface can ask for a password
/// before it offers to mount or restore, rather than after a failure.
#[tauri::command]
pub async fn inspect_backup_image(
    state: State<'_, GuiState>,
    image_path: String,
) -> Result<own_backup_client::ImageInfo, String> {
    state.link.client().await?.inspect_image(&image_path).await
}

/// Restore a volume image onto `target` (DESTRUCTIVE). Progress streams from the
/// service as `restore-progress`.
#[tauri::command]
pub async fn restore_now(
    state: State<'_, GuiState>,
    image_path: String,
    target: String,
    password: Option<String>,
    recovery_key: Option<String>,
) -> Result<RestoreSummaryDto, String> {
    state
        .link
        .client()
        .await?
        .restore_volume(&image_path, &target, &secret_of(password, recovery_key))
        .await
}

/// Verify an image against its sidecar checksums. Progress streams as `verify-progress`.
#[tauri::command]
pub async fn verify_now(
    state: State<'_, GuiState>,
    image_path: String,
    password: Option<String>,
    recovery_key: Option<String>,
) -> Result<VerifySummaryDto, String> {
    state
        .link
        .client()
        .await?
        .verify_image(&image_path, &secret_of(password, recovery_key))
        .await
}

#[tauri::command]
pub async fn mount_backup(
    state: State<'_, GuiState>,
    image_path: String,
    read_write: Option<bool>,
    password: Option<String>,
    recovery_key: Option<String>,
) -> Result<String, String> {
    state
        .link
        .client()
        .await?
        .mount_image(
            &image_path,
            read_write.unwrap_or(false),
            &secret_of(password, recovery_key),
        )
        .await
}

/// Base images the machine can build a boot medium on. Empty is not an error:
/// the interface offers the ways to get one instead (`reagentc /enable`, an
/// installation ISO, pointing at a file).
#[tauri::command]
pub async fn find_base_images(
    state: State<'_, GuiState>,
) -> Result<Vec<own_backup_common::engine_dto::BaseImageDto>, String> {
    let found = state.link.client().await?.find_base_images().await?;
    tracing::info!(count = found.len(), "base images offered");
    Ok(found)
}

/// Driver packages this machine could put on the medium. Sizes included: a
/// display driver can be several hundred megabytes a rescue medium never uses.
#[tauri::command]
pub async fn list_driver_packages(
    state: State<'_, GuiState>,
) -> Result<Vec<own_backup_common::engine_dto::DriverPackageDto>, String> {
    let list = state.link.client().await?.list_drivers().await?;
    tracing::info!(count = list.len(), "driver packages offered");
    Ok(list)
}

/// Third-party tools this machine has that the medium could carry. Nothing is
/// downloaded: the copy would be made from the user's own installation, which
/// is why the wizard asks rather than doing it quietly.
#[tauri::command]
pub async fn list_media_tools(
    state: State<'_, GuiState>,
) -> Result<Vec<own_backup_common::engine_dto::MediaToolDto>, String> {
    let list = state.link.client().await?.list_media_tools().await?;
    tracing::info!(count = list.len(), "third-party tools offered");
    Ok(list)
}

/// Build the boot medium. Minutes of work in the service; progress comes back
/// on the `media-progress` window event.
#[tauri::command]
pub async fn build_boot_media(
    state: State<'_, GuiState>,
    mut options: own_backup_common::engine_dto::MediaBuildOptionsDto,
) -> Result<own_backup_common::engine_dto::MediaBuildReportDto, String> {
    // Whose settings the third-party tools should go onto the medium with.
    // Asked here and nowhere else: this process runs as the person building the
    // medium, and the one that does the building does not — the service is
    // LocalSystem, whose `%APPDATA%` is the SYSTEM profile and holds nobody's
    // Notepad++ configuration. The tools went on the medium freshly installed
    // and the build reported success (2026-09-19).
    if options.user_appdata.trim().is_empty() {
        if let Ok(appdata) = std::env::var("APPDATA") {
            options.user_appdata = appdata;
        }
    }
    tracing::info!(
        iso = %options.output_iso,
        drivers = options.drivers.len(),
        tools = options.tools.len(),
        "building boot medium"
    );
    let report = state.link.client().await?.build_media(&options).await?;
    tracing::info!(mb = report.iso_bytes / 1_048_576, "boot medium written");
    Ok(report)
}

#[tauri::command]
pub async fn unmount_backup(state: State<'_, GuiState>, image_path: String) -> Result<(), String> {
    state.link.client().await?.unmount_image(&image_path).await
}

// --- What mounting a backup image needs here (no driver of ours) ---

/// How this machine boots: the firmware's entries and the loaders sitting on
/// its EFI partitions, compared.
///
/// Read through the service because the privilege it needs
/// (`SeSystemEnvironmentPrivilege`) belongs to a process, and this one is not
/// elevated.
#[tauri::command]
pub async fn boot_status(state: State<'_, GuiState>) -> Result<serde_json::Value, String> {
    state.link.client().await?.boot_status().await
}

/// Change one thing about it. The service takes a copy of the whole
/// configuration first; that is not optional and not ours to skip.
#[tauri::command]
pub async fn boot_action(
    state: State<'_, GuiState>,
    action: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let out = state.link.client().await?.boot_action(&action).await?;
    tracing::info!(action = %action, "boot configuration changed from the interface");
    Ok(out)
}

#[tauri::command]
pub async fn mount_backend_status(state: State<'_, GuiState>) -> Result<serde_json::Value, String> {
    state.link.client().await?.mount_backend_status().await
}

#[tauri::command]
pub async fn prepare_mount_backend(state: State<'_, GuiState>) -> Result<serde_json::Value, String> {
    state.link.client().await?.prepare_mount_backend().await
}

/// Restore a WHOLE disk from a `.obdm` manifest onto `target_disk`. DESTRUCTIVE.
/// The service's guard still refuses the running system disk unless `force` (and
/// even then only outside a recovery environment). Progress: `restore-disk-progress`.
#[tauri::command]
pub async fn restore_disk_now(
    state: State<'_, GuiState>,
    manifest_path: String,
    target_disk: u32,
    force: bool,
    password: Option<String>,
    recovery_key: Option<String>,
) -> Result<DiskRestoreSummaryDto, String> {
    state
        .link
        .client()
        .await?
        .restore_disk(
            &manifest_path,
            target_disk,
            force,
            &secret_of(password, recovery_key),
        )
        .await
}

/// Request cancellation of the running restore/verify/disk-restore in the service.
#[tauri::command]
pub async fn cancel_engine_op(state: State<'_, GuiState>) -> Result<(), String> {
    state.link.client().await?.cancel_engine_op().await
}

// ---------------------------------------------------------------------------
// Restore plans (the partition designer)
// ---------------------------------------------------------------------------

/// Check a plan without writing anything (D5).
///
/// The reply contains the plan **back again**, because the dry run resolves
/// drive-letter conflicts by editing it (D11). The frontend must apply the plan
/// it got back, not the one it sent, or that fix is thrown away.
#[tauri::command]
pub async fn dry_run_restore_plan(
    state: State<'_, GuiState>,
    plan: RestorePlan,
    context: Option<CheckContext>,
    password: Option<String>,
    recovery_key: Option<String>,
) -> Result<serde_json::Value, String> {
    state
        .link
        .client()
        .await?
        .dry_run_restore_plan(
            &plan,
            &context.unwrap_or_default(),
            &secret_of(password, recovery_key),
        )
        .await
}

/// Execute a restore plan. DESTRUCTIVE — the caller is responsible for having
/// shown the dry run's findings and got a confirmation.
///
/// Returns immediately with the run's initial state; progress arrives as
/// `restore-plan-progress`, and [`get_restore_state`] gives the current state
/// at any time (including after the window was closed and reopened).
#[tauri::command]
pub async fn apply_restore_plan(
    state: State<'_, GuiState>,
    plan: RestorePlan,
    passphrases: Option<std::collections::HashMap<u32, String>>,
    // What opens the COPY, when it is encrypted. A different thing from
    // `passphrases`, which put a source volume's own container back.
    password: Option<String>,
    recovery_key: Option<String>,
) -> Result<RestoreRunState, String> {
    state
        .link
        .client()
        .await?
        .apply_restore_plan(
            &plan,
            &passphrases.unwrap_or_default(),
            &secret_of(password, recovery_key),
        )
        .await
}

/// The restore in progress, or the last one that finished. `None` when no plan
/// has run since the service started.
#[tauri::command]
pub async fn get_restore_state(
    state: State<'_, GuiState>,
) -> Result<Option<RestoreRunState>, String> {
    state.link.client().await?.get_restore_state().await
}

/// Ask the named run to stop. The reply says whether it was accepted: while a
/// non-cancellable operation runs the service refuses outright rather than
/// accepting the request and ignoring it.
#[tauri::command]
pub async fn cancel_restore_plan(
    state: State<'_, GuiState>,
    plan_id: String,
) -> Result<serde_json::Value, String> {
    state.link.client().await?.cancel_restore_plan(&plan_id).await
}

/// The user closed the finished run's report — stop handing it back.
#[tauri::command]
pub async fn ack_restore_run(
    state: State<'_, GuiState>,
    plan_id: String,
) -> Result<(), String> {
    state.link.client().await?.ack_restore_run(&plan_id).await
}

/// Check a LUKS passphrase against the header stored in a copy (D19).
///
/// Runs here rather than in the service: it is a pure computation over bytes
/// already read from the image, and the answer is the same one the restore
/// would get. Finding out that a password is wrong should not require starting
/// a restore and waiting for it to reach the container.
///
/// Returns `true` when the passphrase opens the container, `false` when it does
/// not. An `Err` means the check could not be made at all — no stored header,
/// an unreadable image — which is a different thing from a wrong password and
/// is reported as such.
#[tauri::command]
pub async fn verify_luks_password(
    image_path: String,
    src_index: u32,
    password: String,
) -> Result<bool, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let sidecar = load_sidecar(
            std::path::Path::new(&image_path),
            &own_backup_client::ArchiveSecret::none(),
        )?;
        let headers = &sidecar.info.luks_headers;
        // A per-volume image records index 0 — there was nothing to
        // disambiguate at backup time — so its lone entry answers for whichever
        // partition is asked about.
        let header = match headers.as_slice() {
            [only] if sidecar.disk_manifest.is_none() => Some(only),
            _ => headers.iter().find(|h| h.part_index == src_index),
        }
        .ok_or_else(|| {
            format!("в копии не сохранён заголовок контейнера для раздела {src_index}")
        })?;

        own_backup_backup::luks::verify_stored_passphrase(header, &password)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("проверка пароля не запустилась: {e}"))?
}
