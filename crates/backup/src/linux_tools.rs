//! The Linux tools a restore runs on what it wrote: `mkfs.*`, the grow tools,
//! `tune2fs`/`xfs_admin`/`btrfstune`/`cryptsetup`/`vgimportclone`/`mdadm`
//! (PLAN §6, stage 3).
//!
//! Each of them is the filesystem's or volume manager's own maintainer tool —
//! the one place that knows every feature flag and checksum of its on-disk
//! format. Reimplementing a UUID change of a metadata-checksummed ext4 or a
//! btrfs fsid rewrite here would be a second, less tested copy of that
//! knowledge.
//!
//! Calls go through [`Tools`], so the decisions around them (which tool, which
//! arguments, what an exit code means) are tested without running anything.
//! The real runner fixes the locale to `C` and never inherits a terminal: no
//! tool may stop to ask a question on the service's stdin.

#![cfg(target_os = "linux")]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tracing::{debug, info, warn};

use own_backup_common::{tr, OwnBackupError, Result};

/// What a tool printed and how it ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolOutput {
    /// Exit code; `None` when killed by a signal.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl ToolOutput {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }
}

/// Runs external programs.
pub trait Tools {
    /// Run `prog` with `args`; `stdin` is written to its input (and closed),
    /// otherwise the input is empty.
    fn run(
        &mut self,
        prog: &str,
        args: &[String],
        stdin: Option<&[u8]>,
    ) -> std::io::Result<ToolOutput>;
}

/// The machine's programs.
pub struct SystemTools;

impl Tools for SystemTools {
    fn run(
        &mut self,
        prog: &str,
        args: &[String],
        stdin: Option<&[u8]>,
    ) -> std::io::Result<ToolOutput> {
        debug!(prog, ?args, "running");
        let mut child = Command::new(prog)
            .args(args)
            .env("LC_ALL", "C")
            .env("LVM_SUPPRESS_FD_WARNINGS", "1")
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
            // A tool that exits before reading all of it closes the pipe;
            // its exit code says what went wrong.
            let _ = pipe.write_all(input);
        }
        let out = child.wait_with_output()?;
        Ok(ToolOutput {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

/// The Debian package that carries `prog`, for the "not installed" message.
pub fn package_of(prog: &str) -> &'static str {
    match prog {
        "mkfs.ext4" | "e2fsck" | "tune2fs" | "resize2fs" => "e2fsprogs",
        "mkfs.xfs" | "xfs_admin" | "xfs_growfs" | "xfs_db" => "xfsprogs",
        "mkfs.btrfs" | "btrfstune" | "btrfs" => "btrfs-progs",
        "mkfs.vfat" | "fatlabel" => "dosfstools",
        "mkfs.exfat" => "exfatprogs",
        "mkntfs" | "ntfsresize" => "ntfs-3g",
        "cryptsetup" => "cryptsetup-bin",
        "vgimportclone" | "vgchange" | "vgs" | "lvs" => "lvm2",
        "mdadm" => "mdadm",
        "dmsetup" => "dmsetup",
        "mount" | "umount" => "mount",
        _ => "",
    }
}

/// Run `prog` and accept the exit codes `accept` allows; anything else, a
/// missing program or a signal is an error naming the tool and what it said.
pub fn run_checked(
    tools: &mut dyn Tools,
    prog: &str,
    args: &[String],
    stdin: Option<&[u8]>,
    accept: impl Fn(i32) -> bool,
) -> Result<ToolOutput> {
    let out = match tools.run(prog, args, stdin) {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(OwnBackupError::Config(
                tr!(
                    "linux_tools.missing",
                    tool = prog,
                    package = package_of(prog)
                )
                .into_owned(),
            ));
        }
        Err(e) => {
            return Err(OwnBackupError::Device(
                tr!("linux_tools.failed", tool = prog, code = "-", text = e).into_owned(),
            ))
        }
    };
    match out.code {
        Some(c) if accept(c) => {
            info!(prog, ?args, code = c, "tool finished");
            Ok(out)
        }
        code => {
            let text = last_lines(if out.stderr.trim().is_empty() {
                &out.stdout
            } else {
                &out.stderr
            });
            warn!(prog, ?args, ?code, %text, "tool failed");
            Err(OwnBackupError::Device(
                tr!(
                    "linux_tools.failed",
                    tool = prog,
                    code = code
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "signal".into()),
                    text = text
                )
                .into_owned(),
            ))
        }
    }
}

/// [`run_checked`] accepting only 0.
pub fn run_ok(tools: &mut dyn Tools, prog: &str, args: &[String]) -> Result<ToolOutput> {
    run_checked(tools, prog, args, None, |c| c == 0)
}

/// The tail of a tool's output: enough to say why, not a page of progress.
fn last_lines(text: &str) -> String {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    lines[lines.len().saturating_sub(4)..].join(" / ")
}

/// `["a", "b"]` as owned arguments.
pub fn args<const N: usize>(a: [&str; N]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

/// A filesystem mounted for one step under `/run/own-backup/restore-mnt`,
/// unmounted on drop. `nosuid,nodev,noexec`: what is read or edited there is
/// data, never something to run.
pub struct TempMount {
    dir: PathBuf,
}

impl TempMount {
    /// Mount `node` with the first `(fstype, options)` of `types` that takes it.
    pub fn mount(node: &str, types: &[(&str, &str)]) -> Result<Self> {
        let name = Path::new(node)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "part".into());
        let dir = Path::new("/run/own-backup/restore-mnt").join(name);
        std::fs::create_dir_all(&dir)?;
        let mut tried = Vec::new();
        for (fstype, opts) in types {
            let out = Command::new("mount")
                .env("LC_ALL", "C")
                .args(["-t", fstype, "-o", &format!("{opts},nosuid,nodev,noexec")])
                .arg(node)
                .arg(&dir)
                .stdin(Stdio::null())
                .output();
            match out {
                Ok(o) if o.status.success() => {
                    info!(node, fstype, dir = %dir.display(), "mounted for a restore step");
                    return Ok(Self { dir });
                }
                Ok(o) => tried.push(format!(
                    "{fstype}: {}",
                    String::from_utf8_lossy(&o.stderr)
                        .trim()
                        .trim_end_matches('.')
                )),
                Err(e) => tried.push(format!("{fstype}: {e}")),
            }
        }
        let _ = std::fs::remove_dir(&dir);
        Err(OwnBackupError::Device(
            tr!(
                "linux_tools.mount_failed",
                node = node,
                tried = tried.join("; ")
            )
            .into_owned(),
        ))
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Unmount now and say whether it worked; a filesystem whose changes must
    /// be on the disk before the next step is let go this way, not by drop.
    pub fn unmount(mut self) -> Result<()> {
        let ok = Command::new("umount")
            .arg(&self.dir)
            .stdin(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        let dir = std::mem::take(&mut self.dir);
        std::mem::forget(self);
        if ok {
            let _ = std::fs::remove_dir(&dir);
            Ok(())
        } else {
            let _ = Command::new("umount").arg("-l").arg(&dir).status();
            Err(OwnBackupError::Device(
                tr!("linux_tools.umount_failed", dir = dir.display().to_string()).into_owned(),
            ))
        }
    }
}

impl Drop for TempMount {
    fn drop(&mut self) {
        let ok = Command::new("umount")
            .arg(&self.dir)
            .stdin(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            // Busy for a moment (a scan that has not let go): detach it now,
            // the kernel finishes when the last user leaves.
            warn!(dir = %self.dir.display(), "umount refused — detaching lazily");
            let _ = Command::new("umount").arg("-l").arg(&self.dir).status();
        }
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// A [`Tools`] that answers from a script and records every call (tests).
#[cfg(test)]
pub(crate) mod fake {
    use super::*;

    #[derive(Default)]
    pub struct FakeTools {
        pub calls: Vec<(String, Vec<String>, Option<Vec<u8>>)>,
        /// Exit codes by program; programs not listed exit 0.
        pub codes: std::collections::HashMap<String, i32>,
        /// Programs that "are not installed".
        pub missing: Vec<String>,
        /// stdout by program.
        pub stdout: std::collections::HashMap<String, String>,
    }

    impl Tools for FakeTools {
        fn run(
            &mut self,
            prog: &str,
            args: &[String],
            stdin: Option<&[u8]>,
        ) -> std::io::Result<ToolOutput> {
            self.calls
                .push((prog.to_string(), args.to_vec(), stdin.map(<[u8]>::to_vec)));
            if self.missing.iter().any(|m| m == prog) {
                return Err(std::io::ErrorKind::NotFound.into());
            }
            Ok(ToolOutput {
                code: Some(*self.codes.get(prog).unwrap_or(&0)),
                stdout: self.stdout.get(prog).cloned().unwrap_or_default(),
                stderr: format!("{prog} said no"),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeTools;
    use super::*;

    #[test]
    fn exit_codes_are_judged_by_the_caller_and_a_missing_tool_names_its_package() {
        let mut t = FakeTools::default();
        t.codes.insert("e2fsck".into(), 1);
        assert!(run_checked(&mut t, "e2fsck", &args(["-f"]), None, |c| c < 4).is_ok());
        let err = run_ok(&mut t, "e2fsck", &args(["-f"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("e2fsck") && err.contains("said no"), "{err}");

        t.missing.push("mkfs.exfat".into());
        let err = run_ok(&mut t, "mkfs.exfat", &[]).unwrap_err().to_string();
        assert!(err.contains("exfatprogs"), "{err}");
    }

    #[test]
    fn the_real_runner_feeds_stdin_and_reads_both_streams() {
        let out = SystemTools
            .run(
                "sh",
                &args(["-c", "cat; echo err >&2; exit 3"]),
                Some(b"hi"),
            )
            .unwrap();
        assert_eq!(out.code, Some(3));
        assert_eq!(out.stdout, "hi");
        assert_eq!(out.stderr.trim(), "err");
        let out = SystemTools.run("sh", &args(["-c", "cat"]), None).unwrap();
        assert_eq!(out.stdout, "", "no stdin given means an empty one");
    }
}
