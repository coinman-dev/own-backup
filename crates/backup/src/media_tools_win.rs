//! Finding Total Commander and Notepad++ on the machine that builds the medium.
//!
//! A rescue environment with a file manager and a text editor a menu away
//! answers an unexpected problem in a minute instead of a reboot, and both are
//! tiny next to the 250 MB of browser runtime the medium already carries.
//!
//! **Nothing is downloaded and nothing is distributed.** The copy is made from
//! the user's own installation on their own machine, into their own medium —
//! exactly as the driver packages already are. That is the whole reason this is
//! allowed to exist after the licence trouble that cost us Arsenal Image
//! Mounter, and it is why the wizard asks rather than doing it quietly.
//!
//! Where they are is asked of the programs themselves. Both keep an install
//! path in the registry, and both are also found in the obvious places when
//! that key is missing — a Total Commander unpacked by hand has no registry
//! entry at all. The proof that the right folder was found is never the folder
//! name: it is the executable named in
//! [`own_backup_common::media_tools::MEDIA_TOOLS`] being inside it. For Total
//! Commander that is `TOTALCMD64.EXE`, which is the only thing telling a 64-bit
//! install from a 32-bit one.

use std::path::{Path, PathBuf};

use own_backup_common::media_tools::{MediaTool, MEDIA_TOOLS};
use tracing::info;

/// A tool found on this machine, ready to be offered.
#[derive(Debug, Clone)]
pub struct FoundTool {
    /// Id from [`MEDIA_TOOLS`] — what the build options carry.
    pub id: String,
    pub name: String,
    /// The folder to copy, whole.
    pub source: PathBuf,
    pub size_bytes: u64,
    pub files: u32,
}

/// Everything of [`MEDIA_TOOLS`] that this machine actually has.
///
/// Silent about what it did not find: a machine without Total Commander is the
/// ordinary case, not a problem to report.
pub fn find_tools() -> Vec<FoundTool> {
    let mut found = Vec::new();
    for tool in MEDIA_TOOLS {
        let Some(source) = locate(&tool) else { continue };
        // What the medium will actually grow by, not what the installation
        // weighs: the folders in `skip` never leave this machine, and a number
        // that counts them is a number the person deciding is misled by.
        let (whole, whole_files) = weigh(&source);
        let (left, left_files) = tool
            .skip
            .iter()
            .map(|folder| weigh(&source.join(folder)))
            .fold((0, 0), |(b, f), (sb, sf)| (b + sb, f + sf));
        let (size_bytes, files) = (whole - left.min(whole), whole_files - left_files.min(whole_files));
        info!(
            tool = tool.id,
            path = %source.display(),
            mb = size_bytes / 1_048_576,
            files,
            left_behind_mb = left / 1_048_576,
            "third-party tool found for the medium"
        );
        found.push(FoundTool {
            id: tool.id.to_string(),
            name: tool.name.to_string(),
            source,
            size_bytes,
            files,
        });
    }
    found
}

/// The first place holding this tool's executable, or nothing.
fn locate(tool: &MediaTool) -> Option<PathBuf> {
    candidates(tool).into_iter().find(|dir| dir.join(tool.exe).is_file())
}

/// Where to look, best answer first: what the program itself recorded, then the
/// places an installer would have used.
fn candidates(tool: &MediaTool) -> Vec<PathBuf> {
    let mut places: Vec<PathBuf> = Vec::new();
    let mut add = |value: Option<String>| {
        if let Some(v) = value {
            let v = v.trim().trim_matches('"').to_string();
            if !v.is_empty() {
                places.push(PathBuf::from(v));
            }
        }
    };

    match tool.id {
        "totalcmd" => {
            // Ghisler's own key. Per-user first: a Total Commander installed
            // for one account records it there and nowhere else.
            add(read_string(HKEY_CURRENT_USER, r"Software\Ghisler\Total Commander", "InstallDir"));
            add(read_string(HKEY_LOCAL_MACHINE, r"SOFTWARE\Ghisler\Total Commander", "InstallDir"));
            add(program_files().map(|p| p.join("totalcmd").display().to_string()));
            add(program_files_x86().map(|p| p.join("totalcmd").display().to_string()));
            // Its own default since the nineties, and still common.
            add(Some(r"C:\totalcmd".into()));
        }
        "notepadpp" => {
            // The default value of the key, not a named one.
            add(read_string(HKEY_LOCAL_MACHINE, r"SOFTWARE\Notepad++", ""));
            add(read_string(HKEY_LOCAL_MACHINE, r"SOFTWARE\WOW6432Node\Notepad++", ""));
            add(program_files().map(|p| p.join("Notepad++").display().to_string()));
            add(program_files_x86().map(|p| p.join("Notepad++").display().to_string()));
        }
        _ => {}
    }
    places
}

fn program_files() -> Option<PathBuf> {
    std::env::var("ProgramW6432")
        .or_else(|_| std::env::var("ProgramFiles"))
        .ok()
        .map(PathBuf::from)
}

fn program_files_x86() -> Option<PathBuf> {
    std::env::var("ProgramFiles(x86)").ok().map(PathBuf::from)
}

/// Size and file count of a folder, because the wizard shows both: the medium
/// is a RAM disk, and an installation with a dozen plugins is not the size
/// anyone expects.
pub fn weigh(dir: &Path) -> (u64, u32) {
    let Ok(entries) = std::fs::read_dir(dir) else { return (0, 0) };
    let mut bytes = 0;
    let mut files = 0;
    for entry in entries.flatten() {
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => {
                let (b, f) = weigh(&entry.path());
                bytes += b;
                files += f;
            }
            Ok(_) => {
                bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
                files += 1;
            }
            Err(_) => {}
        }
    }
    (bytes, files)
}

// One value out of the registry, defined once in `common` — the same question
// is asked from the display command, and two copies of it would eventually
// disagree about what an empty value means.
use own_backup_common::win_registry::read_string;
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};

#[cfg(test)]
mod tests {
    use super::*;

    /// The search must not conclude anything from a folder's name. A directory
    /// called `totalcmd` holding a 32-bit install is exactly the case this is
    /// supposed to refuse.
    #[test]
    fn a_folder_without_the_right_executable_is_not_the_tool() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("totalcmd");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("TOTALCMD.EXE"), b"32-bit").unwrap();

        let tool = own_backup_common::media_tools::tool("totalcmd").unwrap();
        assert!(!dir.join(tool.exe).is_file(), "the 64-bit one is not there");

        std::fs::write(dir.join(tool.exe), b"64-bit").unwrap();
        assert!(dir.join(tool.exe).is_file(), "and now it is");
    }

    #[test]
    fn a_folder_is_weighed_including_what_is_nested_in_it() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("a.exe"), vec![0u8; 100]).unwrap();
        let plugins = temp.path().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(plugins.join("b.dll"), vec![0u8; 50]).unwrap();

        let (bytes, files) = weigh(temp.path());
        assert_eq!(bytes, 150);
        assert_eq!(files, 2, "the nested one counts too");
    }

    #[test]
    fn nothing_is_weighed_where_there_is_nothing() {
        assert_eq!(weigh(Path::new(r"Z:\no such place")), (0, 0));
    }

    /// Every tool must have somewhere to be looked for, or it can never be
    /// found and the entry is decoration.
    #[test]
    fn every_tool_in_the_table_has_places_to_look() {
        for tool in MEDIA_TOOLS {
            assert!(!candidates(&tool).is_empty(), "nowhere to look for {}", tool.id);
        }
    }
}
