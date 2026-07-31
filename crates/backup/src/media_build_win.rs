//! Building the boot medium (Windows).
//!
//! The layout — what goes where and what the medium does at boot — lives in
//! [`crate::media_layout`] and is tested there. This is the machinery: copy the
//! base image, open it, put the payload in, add this machine's drivers, close
//! it, and lay out the files a firmware can boot.
//!
//! Two things about how it works, both learned the hard way:
//!
//! * **The image is opened by mounting it, or by unpacking it if mounting is
//!   unavailable.** Mounting needs the `wimmount` filter driver, and trimmed
//!   Windows builds ship the driver without registering its service (error
//!   1243). One INF install fixes that, so it is tried before giving up;
//!   unpacking and recapturing is the fallback and costs minutes and gigabytes.
//! * **Drivers come from the machine being used to build.** Their absence is
//!   exactly why "universal" rescue media cannot see NVMe or RAID controllers.

use crate::media_layout as layout;
use crate::wim_info::read_wim_info;
use own_backup_common::{OwnBackupError, Result};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// What to build and how.
#[derive(Debug, Clone)]
pub struct MediaBuildRequest {
    /// Base image found by [`crate::winre_find`].
    pub base_wim: PathBuf,
    /// Folder holding our three exes — normally where the running program is.
    pub program_dir: PathBuf,
    /// WebView2 runtime to copy (the installed Evergreen runtime, or a folder
    /// the user pointed at).
    pub webview2_dir: PathBuf,
    /// Where the medium's files are assembled.
    pub out_dir: PathBuf,
    /// Working directory for the scratch tree; needs a few GB when the image
    /// has to be unpacked.
    pub work_dir: PathBuf,
    /// Export this machine's drivers into the image. Off makes a smaller medium
    /// that may not see this machine's disks.
    pub include_drivers: bool,
    /// An extra folder of drivers (`.inf`) to add as well.
    pub extra_drivers: Option<PathBuf>,
}

/// What the build did, for the report the user reads afterwards.
#[derive(Debug, Clone, Default)]
pub struct MediaBuildReport {
    pub base_name: String,
    pub base_build: u32,
    pub base_language: String,
    /// True when the image had to be unpacked and recaptured.
    pub unpacked: bool,
    pub drivers_added: u32,
    pub runtime_files_trimmed: u32,
    pub image_bytes: u64,
    pub media_bytes: u64,
    pub media_dir: PathBuf,
    pub warnings: Vec<String>,
}

/// Progress phases, so the wizard can show something honest during the minutes
/// this takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildPhase {
    Preparing,
    OpeningImage,
    CopyingPayload,
    AddingDrivers,
    WritingImage,
    LayingOutMedia,
    Done,
}

impl BuildPhase {
    pub fn describe(&self) -> &'static str {
        match self {
            BuildPhase::Preparing => "Подготовка",
            BuildPhase::OpeningImage => "Открытие образа",
            BuildPhase::CopyingPayload => "Копирование программы и WebView2",
            BuildPhase::AddingDrivers => "Добавление драйверов этой машины",
            BuildPhase::WritingImage => "Запись образа",
            BuildPhase::LayingOutMedia => "Сборка носителя",
            BuildPhase::Done => "Готово",
        }
    }
}

/// Build the medium. `progress` is called on every phase change.
pub fn build_media(
    req: &MediaBuildRequest,
    mut progress: impl FnMut(BuildPhase, &str),
) -> Result<MediaBuildReport> {
    let mut report = MediaBuildReport { media_dir: req.out_dir.join("media"), ..Default::default() };

    progress(BuildPhase::Preparing, "проверка исходных файлов");
    let info = read_wim_info(&req.base_wim)?;
    report.base_name = info.name.clone();
    report.base_build = info.build;
    report.base_language = info.language.clone();
    for exe in layout::PROGRAM_FILES {
        if !req.program_dir.join(exe).exists() {
            return Err(OwnBackupError::ImageFormat(format!(
                "{} не найден в {}",
                exe,
                req.program_dir.display()
            )));
        }
    }
    if !req.webview2_dir.join("msedgewebview2.exe").exists() {
        return Err(OwnBackupError::ImageFormat(format!(
            "{}: не похоже на рантайм WebView2 (нет msedgewebview2.exe)",
            req.webview2_dir.display()
        )));
    }

    let work = &req.work_dir;
    let tree = work.join("tree");
    let staged_wim = work.join("boot.wim");
    let _ = std::fs::remove_dir_all(&tree);
    std::fs::create_dir_all(&tree)?;
    std::fs::create_dir_all(&report.media_dir)?;
    std::fs::copy(&req.base_wim, &staged_wim)?;
    clear_readonly(&staged_wim);

    progress(BuildPhase::OpeningImage, "монтирование");
    let mounted = open_image(&staged_wim, &tree)?;
    report.unpacked = !mounted;
    if !mounted {
        progress(BuildPhase::OpeningImage, "монтирование недоступно — распаковка");
    }

    progress(BuildPhase::CopyingPayload, "программа");
    let app_dir = tree.join("Program Files").join("OwnBackup");
    std::fs::create_dir_all(&app_dir)?;
    for exe in layout::PROGRAM_FILES {
        std::fs::copy(req.program_dir.join(exe), app_dir.join(exe))?;
    }

    // The runtime goes on the medium's partition, not into the image: WinPE
    // holds the whole image in RAM, and 250 MB of browser is not needed there
    // until the interface starts.
    progress(BuildPhase::CopyingPayload, "WebView2 (около 250 МБ)");
    let media_app = report.media_dir.join(layout::MEDIA_DIR);
    let runtime_dst = media_app.join("WebView2");
    std::fs::create_dir_all(&runtime_dst)?;
    copy_tree(&req.webview2_dir, &runtime_dst)?;
    std::fs::write(
        media_app.join(layout::MEDIA_MARKER),
        layout::MEDIA_MARKER_TEXT.as_bytes(),
    )?;
    report.runtime_files_trimmed = trim_runtime(&runtime_dst);
    for dll in layout::VC_RUNTIME {
        let src = system32().join(dll);
        if src.exists() {
            let _ = std::fs::copy(&src, runtime_dst.join(dll));
            let x64 = runtime_dst.join("x64");
            if x64.is_dir() {
                let _ = std::fs::copy(&src, x64.join(dll));
            }
        }
    }

    progress(BuildPhase::CopyingPayload, "системные библиотеки");
    let sys32 = tree.join("Windows").join("System32");
    for dll in layout::SYSTEM_DLLS {
        let dst = sys32.join(dll);
        if dst.exists() {
            continue; // never overwrite what the base image already has
        }
        let src = system32().join(dll);
        if src.exists() {
            let _ = std::fs::copy(&src, &dst);
        }
    }
    // Whatever the base carried, this one must not be in the finished image.
    let _ = std::fs::remove_file(sys32.join(layout::FORBIDDEN_DLL));

    let app_dir_win = r"X:\Program Files\OwnBackup";
    let temp_dir_win = r"X:\Windows\Temp\OwnBackup";
    std::fs::write(
        app_dir.join("start-media.cmd"),
        layout::start_media_cmd(app_dir_win, temp_dir_win),
    )?;
    std::fs::write(
        app_dir.join("webview2-args.txt"),
        layout::webview2_args_file(&format!("{temp_dir_win}\\webview2.log")),
    )?;
    // WinRE ships its own winpeshl.ini to launch the recovery menu; ours must
    // not be there at all (see media_layout::startnet_cmd).
    let _ = std::fs::remove_file(sys32.join("winpeshl.ini"));
    std::fs::write(sys32.join("startnet.cmd"), layout::startnet_cmd(app_dir_win))?;

    if req.include_drivers {
        progress(BuildPhase::AddingDrivers, "экспорт драйверов этой машины");
        match add_drivers(&tree, work, req.extra_drivers.as_deref()) {
            Ok(n) => report.drivers_added = n,
            Err(e) => {
                warn!(error = %e, "drivers were not added");
                report.warnings.push(format!(
                    "Драйверы этой машины добавить не удалось: {e}. Носитель соберётся, \
                     но может не увидеть диски на другом оборудовании."
                ));
            }
        }
    }

    set_scratch_space(&tree, mounted);

    progress(BuildPhase::WritingImage, "запись");
    close_image(&staged_wim, &tree, mounted)?;
    nuke_tree(&tree);

    progress(BuildPhase::LayingOutMedia, "загрузочные файлы");
    lay_out_media(&staged_wim, &report.media_dir)?;
    report.image_bytes = std::fs::metadata(report.media_dir.join("sources").join("boot.wim"))
        .map(|m| m.len())
        .unwrap_or(0);
    report.media_bytes = dir_size(&report.media_dir);

    progress(BuildPhase::Done, "готово");
    info!(
        image_mb = report.image_bytes / 1_048_576,
        media_mb = report.media_bytes / 1_048_576,
        drivers = report.drivers_added,
        unpacked = report.unpacked,
        "boot medium built"
    );
    Ok(report)
}

/// Mount the image; fall back to unpacking it. Returns true when mounted.
fn open_image(wim: &Path, tree: &Path) -> Result<bool> {
    if dism(&["/Mount-Wim", &format!("/WimFile:{}", wim.display()), "/Index:1",
              &format!("/MountDir:{}", tree.display())]).is_ok()
    {
        return Ok(true);
    }
    // Error 1243: the driver is there, its service is not registered.
    let inf = system32().join("drivers").join("wimmount.inf");
    if inf.exists() {
        info!("mount failed — registering the wimmount driver and retrying");
        let _ = run("rundll32.exe", &[
            "setupapi.dll,InstallHinfSection",
            "DefaultInstall",
            "132",
            &inf.display().to_string(),
        ]);
        if dism(&["/Mount-Wim", &format!("/WimFile:{}", wim.display()), "/Index:1",
                  &format!("/MountDir:{}", tree.display())]).is_ok()
        {
            return Ok(true);
        }
    }
    warn!("WIM mounting unavailable — unpacking instead (slower, needs a few GB)");
    dism(&["/Apply-Image", &format!("/ImageFile:{}", wim.display()), "/Index:1",
           &format!("/ApplyDir:{}", tree.display())])?;
    Ok(false)
}

fn close_image(wim: &Path, tree: &Path, mounted: bool) -> Result<()> {
    if mounted {
        return dism(&["/Unmount-Wim", &format!("/MountDir:{}", tree.display()), "/Commit"]);
    }
    let _ = std::fs::remove_file(wim);
    dism(&[
        "/Capture-Image",
        &format!("/ImageFile:{}", wim.display()),
        &format!("/CaptureDir:{}", tree.display()),
        "/Name:My Own Backup media",
        "/Compress:max",
        "/Bootable",
    ])
}

/// WinPE's writable space. Set through DISM when mounted; through the image's
/// own registry hive when unpacked, since DISM cannot address an unpacked tree.
fn set_scratch_space(tree: &Path, mounted: bool) {
    let mb = layout::SCRATCH_SPACE_MB;
    if mounted {
        let _ = dism(&[
            &format!("/Image:{}", tree.display()),
            &format!("/Set-ScratchSpace:{mb}"),
        ]);
        return;
    }
    let hive = tree.join(r"Windows\System32\config\SYSTEM");
    if run("reg", &["load", r"HKLM\OwnBackupWpe", &hive.display().to_string()]).is_ok() {
        let _ = run("reg", &[
            "add",
            r"HKLM\OwnBackupWpe\ControlSet001\Services\FBWF",
            "/v", "WinPECacheThreshold", "/t", "REG_DWORD",
            "/d", &mb.to_string(), "/f",
        ]);
        let _ = run("reg", &["unload", r"HKLM\OwnBackupWpe"]);
    }
}

/// Export this machine's drivers and add them to the image. Returns how many
/// `.inf` packages went in.
fn add_drivers(tree: &Path, work: &Path, extra: Option<&Path>) -> Result<u32> {
    let export = work.join("drivers");
    let _ = std::fs::remove_dir_all(&export);
    std::fs::create_dir_all(&export)?;
    run("pnputil", &["/export-driver", "*", &export.display().to_string()])?;

    let mut count = count_inf(&export);
    dism(&[
        &format!("/Image:{}", tree.display()),
        "/Add-Driver",
        &format!("/Driver:{}", export.display()),
        "/Recurse",
    ])?;
    if let Some(dir) = extra.filter(|d| d.is_dir()) {
        if dism(&[
            &format!("/Image:{}", tree.display()),
            "/Add-Driver",
            &format!("/Driver:{}", dir.display()),
            "/Recurse",
        ])
        .is_ok()
        {
            count += count_inf(dir);
        }
    }
    let _ = std::fs::remove_dir_all(&export);
    Ok(count)
}

fn count_inf(dir: &Path) -> u32 {
    fn walk(dir: &Path, n: &mut u32) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, n);
            } else if p.extension().map(|x| x.eq_ignore_ascii_case("inf")).unwrap_or(false) {
                *n += 1;
            }
        }
    }
    let mut n = 0;
    walk(dir, &mut n);
    n
}

/// The boot chain, taken from the running Windows: `bootmgfw.efi` plus the
/// READY-MADE BCD store that ships with it, which already boots
/// `\sources\boot.wim` off a ramdisk. No bcdedit, no oscdimg, no ADK.
fn lay_out_media(wim: &Path, media: &Path) -> Result<()> {
    let boot = PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()))
        .join("Boot");
    std::fs::create_dir_all(media.join(r"EFI\BOOT"))?;
    std::fs::create_dir_all(media.join(r"EFI\Microsoft\Boot"))?;
    std::fs::create_dir_all(media.join("boot"))?;
    std::fs::create_dir_all(media.join("sources"))?;
    std::fs::copy(boot.join(r"EFI\bootmgfw.efi"), media.join(r"EFI\BOOT\BOOTX64.EFI"))?;
    std::fs::copy(boot.join(r"DVD\EFI\BCD"), media.join(r"EFI\Microsoft\Boot\BCD"))?;
    std::fs::copy(boot.join(r"DVD\EFI\boot.sdi"), media.join(r"boot\boot.sdi"))?;
    let dst = media.join(r"sources\boot.wim");
    let _ = std::fs::remove_file(&dst);
    std::fs::rename(wim, &dst).or_else(|_| std::fs::copy(wim, &dst).map(|_| ()))?;
    Ok(())
}

fn trim_runtime(runtime_root: &Path) -> u32 {
    let mut n = 0;
    for path in layout::runtime_trim_list(runtime_root) {
        let ok = if path.is_dir() {
            std::fs::remove_dir_all(&path).is_ok()
        } else {
            std::fs::remove_file(&path).is_ok()
        };
        if ok {
            n += 1;
        }
    }
    n
}

fn system32() -> PathBuf {
    PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()))
        .join("System32")
}

fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            std::fs::create_dir_all(&to)?;
            copy_tree(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

fn dir_size(dir: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    rd.flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            _ => e.metadata().map(|m| m.len()).unwrap_or(0),
        })
        .sum()
}

fn clear_readonly(path: &Path) {
    if let Ok(md) = std::fs::metadata(path) {
        let mut perm = md.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perm.set_readonly(false);
        let _ = std::fs::set_permissions(path, perm);
    }
}

/// Files applied out of a Windows image keep the image's ACLs — owned by
/// TrustedInstaller, administrators read-only. Nothing is locked; there is
/// simply no permission to delete, so ownership is taken first.
fn nuke_tree(tree: &Path) {
    if std::fs::remove_dir_all(tree).is_ok() || !tree.exists() {
        return;
    }
    let _ = dism(&["/Cleanup-Mountpoints"]);
    let _ = run("takeown", &["/f", &tree.display().to_string(), "/r", "/a", "/skipsl"]);
    let _ = run("icacls", &[
        &tree.display().to_string(),
        "/grant", "*S-1-5-32-544:(OI)(CI)F", "/t", "/c", "/q",
    ]);
    if std::fs::remove_dir_all(tree).is_err() {
        warn!(path = %tree.display(), "scratch tree could not be removed");
    }
}

fn dism(args: &[&str]) -> Result<()> {
    run("dism", args)
}

/// Run a console tool without flashing a window, and turn a non-zero exit into
/// an error carrying what it printed — DISM's own message is the useful part.
fn run(program: &str, args: &[&str]) -> Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new(program)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| OwnBackupError::ImageFormat(format!("{program}: {e}")))?;
    if out.status.success() {
        return Ok(());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let tail: Vec<&str> = text.lines().rev().filter(|l| !l.trim().is_empty()).take(3).collect();
    Err(OwnBackupError::ImageFormat(format!(
        "{program} завершился с ошибкой: {}",
        tail.into_iter().rev().collect::<Vec<_>>().join(" ")
    )))
}
