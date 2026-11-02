//! The component store (WinSxS), measured and cleaned by DISM.
//!
//! Disk Cleanup's own handler (`Update Cleanup`) is no use here: on the owner's
//! machine, as the service, it said «0 bytes» in two seconds while DISM said two
//! reclaimable packages and «cleanup recommended» (2026-11-02). DISM is the tool
//! Microsoft documents for this. It runs with `/English`, its documented switch for
//! that, so what is read back is never a translation.
//!
//! DISM does not say how much a cleanup would free. What it does say is what the
//! store keeps that it could do without, which is the most a cleanup can free: that
//! is what an analysis reports, marked as an upper bound. A cleanup reports what the
//! store really shrank by.

use std::sync::atomic::AtomicBool;

/// What an analysis found (`up_to`: at most this much) or a removal freed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Outcome {
    pub bytes: u64,
    pub up_to: bool,
}

/// DISM's report on the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub actual_size: u64,
    pub backups: u64,
    pub cache: u64,
    pub reclaimable_packages: u64,
    pub recommended: bool,
}

impl Report {
    fn cleanable(&self) -> bool {
        self.recommended || self.reclaimable_packages > 0
    }
}

/// `4.03 GB`, `512.5 MB`, `0 bytes`.
fn size(text: &str) -> Option<u64> {
    let mut parts = text.split_whitespace();
    let number: f64 = parts.next()?.replace(',', "").parse().ok()?;
    let unit: u64 = match parts.next()?.to_ascii_lowercase().as_str() {
        "byte" | "bytes" => 1,
        "kb" => 1 << 10,
        "mb" => 1 << 20,
        "gb" => 1 << 30,
        "tb" => 1 << 40,
        _ => return None,
    };
    Some((number * unit as f64) as u64)
}

/// What `dism /English /Online /Cleanup-Image /AnalyzeComponentStore` printed; `None`
/// when a line it always prints is not there, which must not read as «nothing to clean».
pub fn parse(output: &str) -> Option<Report> {
    let value = |name: &str| {
        output.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key.trim() == name).then(|| value.trim())
        })
    };
    Some(Report {
        actual_size: size(value("Actual Size of Component Store")?)?,
        backups: size(value("Backups and Disabled Features")?)?,
        cache: size(value("Cache and Temporary Data")?)?,
        reclaimable_packages: value("Number of Reclaimable Packages")?.parse().ok()?,
        recommended: value("Component Store Cleanup Recommended")?.eq_ignore_ascii_case("yes"),
    })
}

/// How much the store gives up, given its reports before and after a cleanup.
pub fn freed(before: &Report, after: &Report) -> u64 {
    before.actual_size.saturating_sub(after.actual_size)
}

/// An analysis' answer from the report.
pub fn estimate(report: &Report) -> Outcome {
    if report.cleanable() {
        Outcome {
            bytes: report.backups + report.cache,
            up_to: true,
        }
    } else {
        Outcome::default()
    }
}

/// Measure the store, or clean it (`remove`). A cleanup under way is not interrupted:
/// `cancel` is heard only before it starts.
pub fn run(remove: bool, cancel: &AtomicBool) -> std::io::Result<Outcome> {
    #[cfg(windows)]
    {
        win::run(remove, cancel)
    }
    #[cfg(not(windows))]
    {
        let _ = (remove, cancel);
        Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
    }
}

#[cfg(windows)]
mod win {
    use std::io::{self, Read};
    use std::os::windows::process::CommandExt;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use super::{estimate, freed, parse, Outcome, Report};

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    /// From the Windows folder, not from `PATH`.
    fn dism() -> PathBuf {
        std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
            .join(r"System32\dism.exe")
    }

    /// Run DISM and return its output. With `stoppable`, `cancel` ends it.
    fn call(args: &[&str], cancel: &AtomicBool, stoppable: bool) -> io::Result<String> {
        let mut child = Command::new(dism())
            .args(["/English", "/Online", "/Cleanup-Image"])
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()?;
        // Read aside: its progress bars would fill the pipe and stop it.
        let mut out = child.stdout.take().ok_or(io::ErrorKind::BrokenPipe)?;
        let reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = out.read_to_end(&mut bytes);
            String::from_utf8_lossy(&bytes).into_owned()
        });
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if stoppable && cancel.load(Ordering::Relaxed) {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(io::ErrorKind::Interrupted.into());
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        let text = reader.join().unwrap_or_default();
        if status.success() {
            Ok(text)
        } else {
            Err(io::Error::other(format!("dism: {status}")))
        }
    }

    fn analyze(cancel: &AtomicBool) -> io::Result<Report> {
        let text = call(&["/AnalyzeComponentStore"], cancel, true)?;
        parse(&text).ok_or_else(|| io::Error::other("dism: an unexpected report"))
    }

    pub(super) fn run(remove: bool, cancel: &AtomicBool) -> io::Result<Outcome> {
        let before = analyze(cancel)?;
        if !remove {
            return Ok(estimate(&before));
        }
        if estimate(&before).bytes == 0 || cancel.load(Ordering::Relaxed) {
            return Ok(Outcome::default());
        }
        call(&["/StartComponentCleanup"], cancel, false)?;
        Ok(Outcome {
            bytes: freed(&before, &analyze(&AtomicBool::new(false))?),
            up_to: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// As this machine's DISM printed it, progress bars included (2026-11-02).
    const REPORT: &str = "\n\
Deployment Image Servicing and Management tool\n\
Version: 10.0.26100.9549\n\
\n\
Image Version: 10.0.26100.9711\n\
\n\
[==========================100.0%==========================] \n\
\n\
Component Store (WinSxS) information:\n\
\n\
Windows Explorer Reported Size of Component Store : 9.63 GB\n\
\n\
Actual Size of Component Store : 9.52 GB\n\
\n\
    Shared with Windows : 5.48 GB\n\
    Backups and Disabled Features : 4.03 GB\n\
    Cache and Temporary Data :  0 bytes\n\
\n\
Date of Last Cleanup : 2026-10-28 12:06:45\n\
\n\
Number of Reclaimable Packages : 2\n\
Component Store Cleanup Recommended : Yes\n\
\n\
The operation completed successfully.\n";

    const GB: f64 = (1u64 << 30) as f64;

    #[test]
    fn the_report_is_read_as_dism_prints_it() {
        let report = parse(REPORT).unwrap();
        assert_eq!(report.actual_size, (9.52 * GB) as u64);
        assert_eq!(report.backups, (4.03 * GB) as u64);
        assert_eq!(report.cache, 0);
        assert_eq!(report.reclaimable_packages, 2);
        assert!(report.recommended);
    }

    #[test]
    fn an_analysis_gives_an_upper_bound_when_there_is_something_to_clean() {
        let found = estimate(&parse(REPORT).unwrap());
        assert_eq!(found.bytes, (4.03 * GB) as u64);
        assert!(found.up_to);
    }

    #[test]
    fn a_store_with_nothing_to_clean_is_empty_whatever_it_keeps() {
        let text = REPORT
            .replace(
                "Number of Reclaimable Packages : 2",
                "Number of Reclaimable Packages : 0",
            )
            .replace(
                "Component Store Cleanup Recommended : Yes",
                "Component Store Cleanup Recommended : No",
            );
        assert_eq!(estimate(&parse(&text).unwrap()), Outcome::default());
    }

    /// A report in another language or of another form is a failure to see, not an
    /// empty store.
    #[test]
    fn a_report_it_cannot_read_is_not_an_empty_store() {
        assert_eq!(parse("Хранилище компонентов: 9,52 ГБ"), None);
        assert_eq!(
            parse(&REPORT.replace("Cache and Temporary Data :  0 bytes", "")),
            None
        );
        assert_eq!(
            parse(&REPORT.replace("9.52 GB", "9.52 parsecs")),
            None,
            "an unknown unit"
        );
    }

    #[test]
    fn what_a_cleanup_freed_is_what_the_store_shrank_by() {
        let before = parse(REPORT).unwrap();
        let after = Report {
            actual_size: before.actual_size - 1000,
            ..before
        };
        assert_eq!(freed(&before, &after), 1000);
        assert_eq!(freed(&after, &before), 0, "a store that grew freed nothing");
    }

    #[test]
    fn sizes_come_in_every_unit_dism_uses() {
        assert_eq!(size("0 bytes"), Some(0));
        assert_eq!(size("512 KB"), Some(512 << 10));
        assert_eq!(size("1.5 MB"), Some(3 << 19));
        assert_eq!(size("1,024.00 MB"), Some(1 << 30));
        assert_eq!(size("2 TB"), Some(2 << 40));
        assert_eq!(size("nonsense"), None);
    }
}
