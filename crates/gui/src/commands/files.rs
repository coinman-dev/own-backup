//! Browsing the filesystem from inside the interface.
//!
//! The native "choose a folder" dialog is a shell dialog, and WinPE has no
//! shell: on the boot medium the button simply does nothing — no window, no
//! error, the call returns "cancelled" (found 2026-07-31 on the medium, where
//! choosing a destination was therefore impossible). Rather than teach every
//! caller a workaround, the interface can browse by itself, and the same picker
//! serves both environments.
//!
//! Deliberately small: list drives, list one directory, make a directory. No
//! recursion, no watching, no metadata beyond what the list shows.

use serde::Serialize;

#[derive(Serialize)]
pub struct DirEntryDto {
    pub name: String,
    /// Full path, ready to be listed again or handed to the engine.
    pub path: String,
    pub is_dir: bool,
    /// `None` for directories and for files whose size cannot be read.
    pub size: Option<u64>,
    /// Volume label, for drive rows only. A bare list of letters says nothing
    /// about which disk is which — on the medium the user is looking at their
    /// own machine's disks and has to recognise them.
    #[serde(default)]
    pub label: String,
    /// Filesystem of the drive ("NTFS", "FAT32", …). Shown when there is no
    /// label, so a drive is never listed with nothing beside it.
    #[serde(default)]
    pub fs: String,
}

#[derive(Serialize)]
pub struct DirListing {
    /// The directory that was listed. Empty means "the list of drives".
    pub path: String,
    /// Parent directory, or `None` at a drive root (so the UI can offer "up").
    pub parent: Option<String>,
    pub entries: Vec<DirEntryDto>,
}

/// Roots to start from: drive letters on Windows, `/` on Unix.
///
/// Every letter is probed by listing it, because on a boot medium several are
/// present but not readable (an empty card reader, a disconnected network
/// mapping), and offering those only produces an error when clicked.
#[tauri::command]
pub async fn list_roots() -> Result<Vec<DirEntryDto>, String> {
    tokio::task::spawn_blocking(|| {
        #[cfg(target_os = "windows")]
        {
            let mut out = Vec::new();
            for letter in 'A'..='Z' {
                let root = format!("{letter}:\\");
                if std::fs::read_dir(&root).is_ok() {
                    let (label, fs) = volume_label_and_fs(&root);
                    out.push(DirEntryDto {
                        name: format!("{letter}:"),
                        path: root,
                        is_dir: true,
                        size: None,
                        label,
                        fs,
                    });
                }
            }
            Ok(out)
        }
        #[cfg(not(target_os = "windows"))]
        {
            Ok(vec![DirEntryDto {
                name: "/".into(),
                path: "/".into(),
                is_dir: true,
                size: None,
                label: String::new(),
                fs: String::new(),
            }])
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

/// A drive's label and filesystem name, both empty when the volume does not
/// answer (a card reader with no card that still passed the read_dir probe, a
/// mapping that just went away).
#[cfg(target_os = "windows")]
fn volume_label_and_fs(root: &str) -> (String, String) {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::GetVolumeInformationW;

    let wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
    let mut label = [0u16; 256];
    let mut fs = [0u16; 64];
    // SAFETY: `wide` is null-terminated and outlives the call; both buffers are
    // passed with their true lengths.
    let ok = unsafe {
        GetVolumeInformationW(
            PCWSTR(wide.as_ptr()),
            Some(&mut label),
            None,
            None,
            None,
            Some(&mut fs),
        )
    }
    .is_ok();
    if !ok {
        return (String::new(), String::new());
    }
    let cut = |b: &[u16]| {
        let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        String::from_utf16_lossy(&b[..n]).trim().to_string()
    };
    (cut(&label), cut(&fs))
}

/// List one directory: sub-directories first, then files, both by name.
///
/// `files_ext` filters the file half to those extensions (lower-case, no dot);
/// an empty list keeps every file, and a folder picker passes `["\u{0}"]`-free
/// empty vec plus `dirs_only` to hide files entirely.
#[tauri::command]
pub async fn list_dir(
    path: String,
    dirs_only: bool,
    files_ext: Vec<String>,
) -> Result<DirListing, String> {
    tokio::task::spawn_blocking(move || {
        let p = std::path::Path::new(&path);
        let rd = std::fs::read_dir(p).map_err(|e| format!("{path}: {e}"))?;

        let mut dirs = Vec::new();
        let mut files = Vec::new();
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            // Hidden and system entries are noise in a destination picker, and
            // on Windows they are also where the OS keeps things a user has no
            // business writing into.
            if name.starts_with('.') {
                continue;
            }
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue, // a path we cannot even stat is not offerable
            };
            #[cfg(target_os = "windows")]
            {
                use std::os::windows::fs::MetadataExt;
                const HIDDEN: u32 = 0x2;
                const SYSTEM: u32 = 0x4;
                if meta.file_attributes() & (HIDDEN | SYSTEM) != 0 {
                    continue;
                }
            }
            let full = entry.path().to_string_lossy().to_string();
            if meta.is_dir() {
                dirs.push(DirEntryDto {
                    name,
                    path: full,
                    is_dir: true,
                    size: None,
                    label: String::new(),
                    fs: String::new(),
                });
            // Files are listed when one is being picked — and ALSO when a
            // FOLDER is being picked and the caller named the extensions that
            // matter there. Choosing "the folder with the copies" used to show
            // an empty folder: the copies were in it, the folder was the right
            // one, and nothing on screen said so. They cannot be chosen in that
            // mode; they are there to be recognised.
            } else if !dirs_only || !files_ext.is_empty() {
                let keep = files_ext.is_empty()
                    || entry
                        .path()
                        .extension()
                        .map(|e| files_ext.iter().any(|w| w.eq_ignore_ascii_case(&e.to_string_lossy())))
                        .unwrap_or(false);
                if keep {
                    files.push(DirEntryDto {
                        name,
                        path: full,
                        is_dir: false,
                        size: Some(meta.len()),
                        label: String::new(),
                        fs: String::new(),
                    });
                }
            }
        }
        dirs.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        files.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        dirs.append(&mut files);

        Ok(DirListing {
            path: path.clone(),
            parent: p.parent().map(|x| x.to_string_lossy().to_string()).filter(|s| !s.is_empty()),
            entries: dirs,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Create a directory (with parents) so a destination can be made on the spot —
/// on the medium there is no Explorer to go and make one in.
#[tauri::command]
pub async fn create_dir(path: String) -> Result<String, String> {
    tokio::task::spawn_blocking(move || {
        std::fs::create_dir_all(&path).map_err(|e| format!("{path}: {e}"))?;
        Ok(path)
    })
    .await
    .map_err(|e| e.to_string())?
}
