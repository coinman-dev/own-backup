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
    /// What the medium starts it with.
    ///
    /// Total Commander opens the drive it was last looking at, which on a
    /// machine whose settings these are means `C:` — and on a rescue medium
    /// `C:` is the patient: locked by BitLocker as often as not, and then the
    /// file manager opens with an error instead of a panel. `X:` is the RAM
    /// disk, it is always there and it is always readable.
    ///
    /// Done with the program's own switches rather than by editing the
    /// `wincmd.ini` we copied: the settings are the person's, and a medium has
    /// no business rewriting them to say something they did not.
    pub args: &'static [&'static str],
    /// Subfolders of the installation not worth carrying.
    ///
    /// The medium is loaded into RAM whole, so every megabyte here is a
    /// megabyte the rescued machine never gets back — and these are megabytes
    /// nothing on a medium can use. They are left out of the size shown in the
    /// wizard too: the number a person decides on should be the number the
    /// medium actually grows by.
    pub skip: &'static [&'static str],
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
        // Both panels on the RAM disk. /L and /R are Total Commander's own.
        args: &[r"/L=X:\", r"/R=X:\"],
        skip: &[],
    },
    MediaTool {
        id: "notepadpp",
        name: "Notepad++",
        folder: "Notepad++",
        exe: "notepad++.exe",
        portable_marker: Some("doLocalConf.xml"),
        settings_in_appdata: Some("Notepad++"),
        args: &[],
        // `updater` is WinGUP, which downloads new versions — there is no
        // network worth using on a rescue medium and no point updating a copy
        // that lives for one session. `themes` is only read when a theme is
        // chosen instead of the default styling, and the styling itself comes
        // from `stylers.xml`, which IS carried. Together they are a third of
        // the program: 6.9 MiB of 19.9.
        skip: &["themes", "updater"],
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

    /// Is this subfolder of the installation one the medium leaves behind?
    pub fn skips(&self, folder: &str) -> bool {
        self.skip.iter().any(|s| s.eq_ignore_ascii_case(folder))
    }
}

/// The tool with this id, if there is one.
pub fn tool(id: &str) -> Option<&'static MediaTool> {
    MEDIA_TOOLS.iter().find(|t| t.id == id)
}

// --- the Linux medium ----------------------------------------------------------

/// A program the Linux medium may carry: a file manager and a text editor, the
/// counterparts of Total Commander and Notepad++ on WinPE.
///
/// **Not copied from this machine, unlike on Windows.** The Linux medium is a
/// Debian system built from Debian's archive, and these are Debian packages
/// installed into it by the same build, from the same mirror, as everything
/// else on it. The builder only adds their names to the package list; the
/// strip on the booted medium puts a line in the Start menu when [`exe`] is
/// there.
///
/// Everything on the medium runs as root (SYSTEM's place on WinPE), and that
/// rules out KDE's own editor: Kate and KWrite refuse to start as root since
/// 2017. FeatherPad is a Qt editor of the Notepad++ kind (tabs, syntax
/// colours, encodings) that does not, and its Qt is Krusader's anyway.
///
/// [`exe`]: LinuxMediaTool::exe
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinuxMediaTool {
    /// Stable name used in the build options and nowhere a person reads.
    pub id: &'static str,
    /// What the menu and the wizard call it.
    pub name: &'static str,
    /// Debian packages that make it work on the medium. The build installs
    /// without Recommends, so what a program needs to be usable rather than
    /// merely installed is named here: the Wayland platform plugin (without
    /// it a Qt program looks for an X server the medium does not have and
    /// does not start) and, for Krusader, an icon theme with Qt's SVG
    /// plugins to draw it (Breeze is SVG: without them its toolbars and file
    /// lists have no icons at all — stand, 27.10).
    pub packages: &'static [&'static str],
    /// What the Start menu runs, and the proof that the package is there.
    pub exe: &'static str,
}

/// Everything the Linux medium knows how to carry, in the order the menu
/// shows them.
pub const LINUX_MEDIA_TOOLS: [LinuxMediaTool; 2] = [
    LinuxMediaTool {
        id: "krusader",
        name: "Krusader",
        packages: &[
            "krusader",
            "qt6-wayland",
            "breeze-icon-theme",
            "qt6-svg-plugins",
        ],
        exe: "/usr/bin/krusader",
    },
    LinuxMediaTool {
        id: "featherpad",
        name: "FeatherPad",
        // `featherpad-l10n` is its Russian: a Recommends, so named.
        packages: &["featherpad", "featherpad-l10n", "qt6-wayland"],
        exe: "/usr/bin/featherpad",
    },
];

/// The Linux tool with this id, if there is one.
pub fn linux_tool(id: &str) -> Option<&'static LinuxMediaTool> {
    LINUX_MEDIA_TOOLS.iter().find(|t| t.id == id)
}

/// The Debian packages for these tool ids, each once, in table order — or the
/// first id nobody knows. A build is never given a package name from outside
/// this table.
pub fn linux_tool_packages<S: AsRef<str>>(ids: &[S]) -> Result<Vec<&'static str>, String> {
    if let Some(unknown) = ids.iter().find(|id| linux_tool(id.as_ref()).is_none()) {
        return Err(unknown.as_ref().to_string());
    }
    let mut out: Vec<&'static str> = Vec::new();
    for tool in LINUX_MEDIA_TOOLS
        .iter()
        .filter(|t| ids.iter().any(|id| id.as_ref() == t.id))
    {
        for p in tool.packages {
            if !out.contains(p) {
                out.push(p);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_path_the_shell_looks_at_is_the_one_the_builder_writes() {
        assert_eq!(
            tool("totalcmd").unwrap().path_on_medium(),
            r"totalcmd\TOTALCMD64.EXE"
        );
        assert_eq!(
            tool("notepadpp").unwrap().path_on_medium(),
            r"Notepad++\notepad++.exe"
        );
    }

    /// A 32-bit install has `TOTALCMD.EXE` and not this one. That difference is
    /// the whole of the test for "is this the 64-bit one", so it must not be
    /// quietly softened to a folder name or a registry key.
    #[test]
    fn total_commander_is_recognised_by_its_64_bit_executable() {
        let tc = tool("totalcmd").unwrap();
        assert_eq!(tc.exe, "TOTALCMD64.EXE");
        assert!(
            !tc.exe.eq_ignore_ascii_case("TOTALCMD.EXE"),
            "that is the 32-bit one"
        );
    }

    #[test]
    fn every_tool_is_fully_described_and_named_once() {
        let mut ids: Vec<&str> = MEDIA_TOOLS.iter().map(|t| t.id).collect();
        ids.sort_unstable();
        let mut unique = ids.clone();
        unique.dedup();
        assert_eq!(ids, unique, "two tools share an id");
        for t in MEDIA_TOOLS {
            assert!(
                !t.name.is_empty() && !t.folder.is_empty() && !t.exe.is_empty(),
                "{t:?}"
            );
            assert!(
                !t.folder.contains('/') && !t.folder.contains('\\'),
                "one folder, not a path"
            );
        }
    }

    /// The medium is a RAM disk under a different drive letter every boot, so
    /// a program that keeps its settings in the user's profile keeps them
    /// nowhere. Notepad++ has a documented cure and must get it.
    #[test]
    fn notepad_is_told_to_keep_its_settings_beside_itself() {
        assert_eq!(
            tool("notepadpp").unwrap().portable_marker,
            Some("doLocalConf.xml")
        );
    }

    /// Neither of these keeps its settings in its own folder, so a medium that
    /// copies only the installation carries a program nobody recognises.
    #[test]
    fn both_tools_know_where_their_settings_live() {
        for t in MEDIA_TOOLS {
            assert!(
                t.settings_in_appdata.is_some(),
                "{} has nowhere to take settings from",
                t.id
            );
        }
    }

    /// The medium is held in RAM whole, and these two folders are a third of
    /// Notepad++ that nothing on a medium reads: an updater with no network
    /// worth using, and themes that are only consulted when one is chosen
    /// instead of the default styling — which comes from `stylers.xml`.
    #[test]
    fn the_folders_a_medium_has_no_use_for_are_left_behind() {
        let npp = tool("notepadpp").unwrap();
        assert!(npp.skips("themes") && npp.skips("updater"));
        assert!(
            npp.skips("Themes"),
            "a folder name is not case-sensitive on Windows"
        );
        assert!(!npp.skips("plugins"), "those are part of the program");
        assert!(!npp.skips(""), "and nothing matches nothing");
    }

    /// On a rescue medium `C:` is the patient — locked by BitLocker as often as
    /// not — and a file manager that opens there greets the person with an
    /// error instead of a panel. `X:` is the RAM disk: always present, always
    /// readable.
    #[test]
    fn the_file_manager_opens_on_the_ram_disk_and_not_on_the_patient() {
        let tc = tool("totalcmd").unwrap();
        assert_eq!(tc.args, [r"/L=X:\", r"/R=X:\"], "both panels");
        assert!(
            !tc.args.iter().any(|a| a.contains("C:")),
            "never the machine being rescued"
        );
    }

    #[test]
    fn an_id_nobody_knows_is_not_a_tool() {
        assert!(tool("winamp").is_none());
        assert!(tool("").is_none());
        assert!(linux_tool("winamp").is_none());
        assert!(linux_tool("totalcmd").is_none(), "a Windows program");
    }

    /// Both together share Qt's Wayland plugin; it is installed once, and the
    /// order is the table's whatever order the ids came in.
    #[test]
    fn the_linux_tools_become_package_names_each_once() {
        assert_eq!(
            linux_tool_packages(&["featherpad", "krusader"]).unwrap(),
            [
                "krusader",
                "qt6-wayland",
                "breeze-icon-theme",
                "qt6-svg-plugins",
                "featherpad",
                "featherpad-l10n"
            ]
        );
        assert_eq!(
            linux_tool_packages(&["featherpad"]).unwrap(),
            ["featherpad", "featherpad-l10n", "qt6-wayland"]
        );
        assert!(linux_tool_packages::<&str>(&[]).unwrap().is_empty());
    }

    /// The names go into the medium's package list as root: one that is not
    /// in the table never gets that far.
    #[test]
    fn an_unknown_linux_tool_refuses_the_whole_list() {
        assert_eq!(
            linux_tool_packages(&["krusader", "evil; rm -rf /"]),
            Err("evil; rm -rf /".to_string())
        );
    }

    /// The Qt programs do not start without the Wayland plugin on a medium
    /// that has no X server, and the build installs no Recommends.
    #[test]
    fn every_linux_tool_brings_what_it_needs_to_start() {
        for t in LINUX_MEDIA_TOOLS {
            assert!(t.packages.contains(&"qt6-wayland"), "{}", t.id);
            assert!(t.exe.starts_with("/usr/bin/"), "{}", t.id);
            assert!(
                t.packages.iter().all(|p| p
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"+-.".contains(&b))),
                "{}: a Debian package name",
                t.id
            );
        }
        let krusader = linux_tool("krusader").unwrap().packages;
        assert!(krusader.contains(&"breeze-icon-theme"));
        assert!(
            krusader.contains(&"qt6-svg-plugins"),
            "Breeze is SVG; without the plugins no icon is drawn"
        );
    }

    /// Kate refuses to start as root, and everything on the medium is root.
    #[test]
    fn the_linux_editor_is_not_one_that_refuses_root() {
        for t in LINUX_MEDIA_TOOLS {
            assert!(
                !t.packages.iter().any(|p| *p == "kate" || *p == "kwrite"),
                "{}",
                t.id
            );
        }
    }
}
