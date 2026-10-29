//! Linux system work done by the system's own commands (PLAN §6б, stage 8):
//! the journal (`journalctl --vacuum-size`), old snap revisions (`snap
//! remove --revision`), unused Flatpak runtimes (`flatpak uninstall
//! --unused`) and unneeded packages (`apt-get autoremove --purge`).
//! Removing their files by hand would leave the tools' own records wrong.
//!
//! Commands run with `LC_ALL=C`: what is read back is the C locale's text,
//! never a translation. A command that is not on the system means nothing of
//! its kind to clean (`NotFound`).

use std::sync::atomic::AtomicBool;

/// What a handler would free (analysis) or freed (removal).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Outcome {
    pub bytes: u64,
}

/// The journal is cut to this.
pub const JOURNAL_KEEP: u64 = 200 * 1024 * 1024;

pub fn run(name: &str, remove: bool, cancel: &AtomicBool) -> std::io::Result<Outcome> {
    #[cfg(target_os = "linux")]
    {
        let _ = cancel;
        match name {
            "journal" => linux::journal(remove),
            "snap_revisions" => linux::snap_revisions(remove),
            "flatpak_unused" => linux::flatpak_unused(remove),
            "apt_autoremove" => linux::apt_autoremove(remove),
            _ => Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (name, remove, cancel);
        Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
    }
}

/// `snap list --all`'s disabled revisions: (name, revision).
pub fn disabled_snaps(listing: &str) -> Vec<(String, String)> {
    listing
        .lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            // Name Version Rev Tracking Publisher Notes
            (f.len() >= 6 && f[5..].iter().any(|n| n.split(',').any(|n| n == "disabled")))
                .then(|| (f[0].to_string(), f[2].to_string()))
        })
        .collect()
}

/// The refs `flatpak uninstall --unused` lists before it asks: (id, branch).
/// A row is `" 1.\t[flags]\tID\tBRANCH\tOP"` (Flatpak 1.16).
pub fn unused_flatpaks(listing: &str) -> Vec<(String, String)> {
    listing
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').map(str::trim).collect();
            let numbered = f.first().is_some_and(|n| {
                n.strip_suffix('.')
                    .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
            });
            (numbered && f.len() >= 5 && f[f.len() - 1] == "r")
                .then(|| (f[f.len() - 3].to_string(), f[f.len() - 2].to_string()))
        })
        .collect()
}

/// The packages `apt-get -s autoremove` would remove.
pub fn autoremoved(simulation: &str) -> Vec<String> {
    simulation
        .lines()
        .filter_map(|l| l.strip_prefix("Purg ").or_else(|| l.strip_prefix("Remv ")))
        .filter_map(|rest| rest.split_whitespace().next())
        .map(str::to_string)
        .collect()
}

/// Total size of the files below `dir`, links not followed.
pub fn size_of(dir: &std::path::Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(dir) else {
        return 0;
    };
    if meta.file_type().is_symlink() {
        return 0;
    }
    if !meta.is_dir() {
        return meta.len();
    }
    std::fs::read_dir(dir)
        .map(|entries| entries.flatten().map(|e| size_of(&e.path())).sum())
        .unwrap_or(0)
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io;
    use std::path::Path;
    use std::process::Command;

    use super::{autoremoved, disabled_snaps, size_of, unused_flatpaks, Outcome, JOURNAL_KEEP};

    fn command(program: &str) -> Command {
        let mut c = Command::new(program);
        c.env("LC_ALL", "C").env("LANG", "C");
        c
    }

    /// stdout of a command that must succeed; a missing program is
    /// `NotFound`.
    fn output(c: &mut Command) -> io::Result<String> {
        let out = c.output()?;
        if !out.status.success() {
            return Err(io::Error::other(format!(
                "{:?}: {}",
                c.get_program(),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    pub(super) fn journal(remove: bool) -> io::Result<Outcome> {
        let dir = Path::new("/var/log/journal");
        if !dir.is_dir() {
            return Err(io::ErrorKind::NotFound.into());
        }
        let before = size_of(dir);
        if !remove {
            return Ok(Outcome {
                bytes: before.saturating_sub(JOURNAL_KEEP),
            });
        }
        output(command("journalctl").arg(format!("--vacuum-size={}M", JOURNAL_KEEP >> 20)))?;
        Ok(Outcome {
            bytes: before.saturating_sub(size_of(dir)),
        })
    }

    fn snap_file(name: &str, rev: &str) -> std::path::PathBuf {
        Path::new("/var/lib/snapd/snaps").join(format!("{name}_{rev}.snap"))
    }

    pub(super) fn snap_revisions(remove: bool) -> io::Result<Outcome> {
        let listing = output(command("snap").args(["list", "--all"]))?;
        let mut bytes = 0;
        for (name, rev) in disabled_snaps(&listing) {
            let size = std::fs::metadata(snap_file(&name, &rev))
                .map(|m| m.len())
                .unwrap_or(0);
            if remove {
                if let Err(e) =
                    output(command("snap").args(["remove", &name, &format!("--revision={rev}")]))
                {
                    tracing::info!(snap = %name, %rev, error = %e, "cleanup: snap revision left");
                    continue;
                }
            }
            bytes += size;
        }
        Ok(Outcome { bytes })
    }

    const FLATPAK: &str = "/var/lib/flatpak";

    /// Every file below `dir` by inode, links not followed.
    fn inodes(dir: &Path, into: &mut std::collections::HashMap<(u64, u64), u64>) {
        use std::os::unix::fs::MetadataExt;
        let Ok(meta) = std::fs::symlink_metadata(dir) else {
            return;
        };
        if meta.is_dir() {
            for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                inodes(&e.path(), into);
            }
        } else if meta.is_file() {
            into.insert((meta.dev(), meta.ino()), meta.len());
        }
    }

    /// The deployed refs: `<kind>/<id>/<arch>/<branch>`.
    fn deployed() -> Vec<(String, std::path::PathBuf)> {
        let mut found = Vec::new();
        let list = |p: &Path| -> Vec<std::path::PathBuf> {
            std::fs::read_dir(p)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir() && !p.is_symlink())
                .collect()
        };
        for kind in ["app", "runtime"] {
            for id in list(&Path::new(FLATPAK).join(kind)) {
                for arch in list(&id) {
                    for branch in list(&arch) {
                        let name = |p: &Path| {
                            p.file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .into_owned()
                        };
                        found.push((format!("{kind}/{}/{}", name(&id), name(&branch)), branch));
                    }
                }
            }
        }
        found
    }

    /// No dry run: asked without a terminal and answered "n", flatpak lists
    /// what it would remove. Its files are hard links into the shared repo,
    /// so what goes is the files no remaining app or runtime also has.
    pub(super) fn flatpak_unused(remove: bool) -> io::Result<Outcome> {
        use std::io::Write;
        use std::process::Stdio;
        let mut asked = command("flatpak")
            .args(["uninstall", "--unused"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        if let Some(mut stdin) = asked.stdin.take() {
            let _ = stdin.write_all(b"n\n");
        }
        let listing = asked.wait_with_output()?;
        let unused: Vec<String> = unused_flatpaks(&String::from_utf8_lossy(&listing.stdout))
            .into_iter()
            .map(|(id, branch)| format!("runtime/{id}/{branch}"))
            .collect();
        if unused.is_empty() {
            return Ok(Outcome::default());
        }
        let (mut going, mut staying) = Default::default();
        for (name, dir) in deployed() {
            inodes(
                &dir,
                if unused.contains(&name) {
                    &mut going
                } else {
                    &mut staying
                },
            );
        }
        let going: std::collections::HashMap<(u64, u64), u64> = going;
        let bytes = going
            .iter()
            .filter(|(ino, _)| !staying.contains_key(*ino))
            .map(|(_, len)| len)
            .sum();
        if remove {
            output(command("flatpak").args(["uninstall", "--unused", "-y", "--noninteractive"]))?;
        }
        Ok(Outcome { bytes })
    }

    /// Installed size of `package`, from dpkg (KiB).
    fn installed_size(package: &str) -> u64 {
        output(command("dpkg-query").args(["-W", "-f=${Installed-Size}", package]))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map_or(0, |kib| kib * 1024)
    }

    pub(super) fn apt_autoremove(remove: bool) -> io::Result<Outcome> {
        let simulated = output(command("apt-get").args(["-s", "autoremove", "--purge"]))?;
        let packages = autoremoved(&simulated);
        let bytes: u64 = packages.iter().map(|p| installed_size(p)).sum();
        if remove && !packages.is_empty() {
            output(
                command("apt-get")
                    .env("DEBIAN_FRONTEND", "noninteractive")
                    .args(["-y", "autoremove", "--purge"]),
            )?;
        }
        Ok(Outcome { bytes })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_snap_revisions_are_read() {
        let listing = "Name      Version   Rev    Tracking       Publisher   Notes\n\
                       core22    20240111  1122   latest/stable  canonical✓  base,disabled\n\
                       core22    20240408  1380   latest/stable  canonical✓  base\n\
                       firefox   124.0     4033   latest/stable  mozilla✓    disabled\n\
                       firefox   125.0     4173   latest/stable  mozilla✓    -\n";
        assert_eq!(
            disabled_snaps(listing),
            vec![
                ("core22".to_string(), "1122".to_string()),
                ("firefox".to_string(), "4033".to_string())
            ]
        );
    }

    /// Flatpak 1.16.6's listing on the Linux stand (Ubuntu 26.04).
    #[test]
    fn unused_flatpak_runtimes_are_read() {
        let listing = "\n\n 1.\t   \torg.freedesktop.Platform.GL.default\t24.08\tr\n \
                       2.\t   \torg.freedesktop.Platform.GL.default\t24.08extra\tr\n \
                       3.\t   \torg.freedesktop.Platform.Locale\t24.08\tr\n\n\
                       Proceed with these changes to the system installation? [Y/n]: n\n";
        assert_eq!(
            unused_flatpaks(listing),
            vec![
                (
                    "org.freedesktop.Platform.GL.default".to_string(),
                    "24.08".to_string()
                ),
                (
                    "org.freedesktop.Platform.GL.default".to_string(),
                    "24.08extra".to_string()
                ),
                (
                    "org.freedesktop.Platform.Locale".to_string(),
                    "24.08".to_string()
                ),
            ]
        );
        assert!(unused_flatpaks("Nothing unused to uninstall\n").is_empty());
    }

    #[test]
    fn autoremoved_packages_are_read() {
        let sim = "Reading package lists...\n\
                   The following packages will be REMOVED:\n  linux-image-6.8.0-31-generic*\n\
                   Purg linux-image-6.8.0-31-generic [6.8.0-31.31]\n\
                   Remv libfoo1 [1.2-3]\n";
        assert_eq!(
            autoremoved(sim),
            vec!["linux-image-6.8.0-31-generic", "libfoo1"]
        );
    }
}
