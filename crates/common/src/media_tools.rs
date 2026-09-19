//! Third-party programs the boot medium can carry, and where they end up on it.
//!
//! This table is an agreement between two halves that never see each other. The
//! builder copies a folder from the machine making the medium; the shell on the
//! booted medium looks for an executable and puts a line in the Start menu if
//! it is there. Neither can check the other, so the path they would have
//! disagreed about lives here instead, once.
//!
//! **Nothing is distributed.** The copy is made on the user's own machine, from
//! their own installation, exactly as the driver packages already are
//! (`pnputil /export-driver`). The medium is theirs and stays theirs — which is
//! why this can exist at all, after the licence trouble that cost us Arsenal
//! Image Mounter.
//!
//! These are small and they earn their place: a rescue environment where a
//! file manager and a text editor are a menu away answers an unexpected problem
//! in a minute rather than a reboot.

/// A program the medium may carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaTool {
    /// Stable name used in the build options and nowhere a person reads.
    pub id: &'static str,
    /// What the menu and the wizard call it.
    pub name: &'static str,
    /// Its folder on the medium, under [`TOOLS_DIR`].
    pub folder: &'static str,
    /// The executable inside that folder — and, on the machine being searched,
    /// the proof that the right thing was found.
    pub exe: &'static str,
    /// A file that tells the program to keep its settings BESIDE ITSELF
    /// instead of in the user's profile, created on the medium if named.
    ///
    /// This is the difference between a program that works on the medium and
    /// one that half-works. Left to itself, Notepad++ keeps its configuration
    /// in `%APPDATA%` — which in WinPE is the SYSTEM account's profile on a RAM
    /// disk, nothing like the folder its settings were copied from. Its own
    /// documented answer is this marker file: with it present, everything is
    /// read from and written to its own folder, and the drive letter stops
    /// mattering at all.
    pub portable_marker: Option<&'static str>,
    /// Folder under `%APPDATA%` where this program keeps the settings a person
    /// has actually changed, copied onto the medium beside the program itself.
    ///
    /// Without this the medium carries a freshly installed program, not the
    /// one the user knows: neither of these two keeps its settings in its own
    /// folder. Only the top-level `.xml` and `.ini` are taken — the rest of
    /// what lives there is session state and unsaved documents, which are
    /// someone's private files and have no business on a boot medium.
    pub settings_in_appdata: Option<&'static str>,
}

/// Everything the medium knows how to carry, in the order the menu shows them.
///
/// **`TOTALCMD64.EXE` is how the 64-bit Total Commander is told from the
/// 32-bit one**, and there is no other marker in the folder: both installs look
/// identical otherwise, and a 32-bit build has no business on a 64-bit WinPE.
/// The same principle holds for the others — the executable named here is both
/// what gets launched and what the search is looking for.
pub const MEDIA_TOOLS: [MediaTool; 2] = [
    MediaTool {
        id: "totalcmd",
        name: "Total Commander",
        folder: "totalcmd",
        exe: "TOTALCMD64.EXE",
        // None needed: Total Commander is told where its `wincmd.ini` is by the
        // registry, and a medium has no such entry — so it falls back to its
        // own folder, which is where we want it anyway.
        portable_marker: None,
        // `wincmd.ini`. Total Commander looks for it beside the exe when the
        // registry names no other place, and a medium's registry names none.
        settings_in_appdata: Some("GHISLER"),
    },
    MediaTool {
        id: "notepadpp",
        name: "Notepad++",
        folder: "Notepad++",
        exe: "notepad++.exe",
        portable_marker: Some("doLocalConf.xml"),
        settings_in_appdata: Some("Notepad++"),
    },
];

impl MediaTool {
    /// Path to the executable on the booted medium, relative to
    /// `X:\Program Files` — what the desktop joins onto its own parent folder.
    ///
    /// Straight under `Program Files`, beside our own folder rather than inside
    /// a wrapper of ours: `X:\Program Files\totalcmd\TOTALCMD64.EXE` is where
    /// anyone would look for Total Commander on any Windows, and a rescue
    /// medium is the last place to make someone hunt for a file manager.
    pub fn path_on_medium(&self) -> String {
        format!(r"{}\{}", self.folder, self.exe)
    }
}

/// The tool with this id, if there is one.
pub fn tool(id: &str) -> Option<&'static MediaTool> {
    MEDIA_TOOLS.iter().find(|t| t.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_path_the_shell_looks_at_is_the_one_the_builder_writes() {
        assert_eq!(tool("totalcmd").unwrap().path_on_medium(), r"totalcmd\TOTALCMD64.EXE");
        assert_eq!(tool("notepadpp").unwrap().path_on_medium(), r"Notepad++\notepad++.exe");
    }

    /// A 32-bit install has `TOTALCMD.EXE` and not this one. That difference is
    /// the whole of the test for "is this the 64-bit one", so it must not be
    /// quietly softened to a folder name or a registry key.
    #[test]
    fn total_commander_is_recognised_by_its_64_bit_executable() {
        let tc = tool("totalcmd").unwrap();
        assert_eq!(tc.exe, "TOTALCMD64.EXE");
        assert!(!tc.exe.eq_ignore_ascii_case("TOTALCMD.EXE"), "that is the 32-bit one");
    }

    #[test]
    fn every_tool_is_fully_described_and_named_once() {
        let mut ids: Vec<&str> = MEDIA_TOOLS.iter().map(|t| t.id).collect();
        ids.sort_unstable();
        let mut unique = ids.clone();
        unique.dedup();
        assert_eq!(ids, unique, "two tools share an id");
        for t in MEDIA_TOOLS {
            assert!(!t.name.is_empty() && !t.folder.is_empty() && !t.exe.is_empty(), "{t:?}");
            assert!(!t.folder.contains('/') && !t.folder.contains('\\'), "one folder, not a path");
        }
    }

    /// The medium is a RAM disk under a different drive letter every boot, so
    /// a program that keeps its settings in the user's profile keeps them
    /// nowhere. Notepad++ has a documented cure and must get it.
    #[test]
    fn notepad_is_told_to_keep_its_settings_beside_itself() {
        assert_eq!(tool("notepadpp").unwrap().portable_marker, Some("doLocalConf.xml"));
    }

    /// Neither of these keeps its settings in its own folder, so a medium that
    /// copies only the installation carries a program nobody recognises.
    #[test]
    fn both_tools_know_where_their_settings_live() {
        for t in MEDIA_TOOLS {
            assert!(t.settings_in_appdata.is_some(), "{} has nowhere to take settings from", t.id);
        }
    }

    #[test]
    fn an_id_nobody_knows_is_not_a_tool() {
        assert!(tool("winamp").is_none());
        assert!(tool("").is_none());
    }
}
