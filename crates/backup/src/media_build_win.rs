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

/// One driver package published on this machine.
///
/// Sizes are here because they are the answer to "why is the medium 2 GB": a
/// display driver alone can be several hundred megabytes, and a rescue medium
/// has no use for it. The user sees what each one costs before choosing.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DriverPackage {
    /// Published name — `oem12.inf`, what `pnputil` exports by.
    pub inf: String,
    /// The name the vendor gave it, which is the one that means anything.
    pub original_name: String,
    /// Device class: `SCSIAdapter`, `Net`, `Display`, `Printer`, …
    pub class_name: String,
    pub provider: String,
    pub version: String,
    pub size_bytes: u64,
    /// Whether a rescue medium plausibly needs it — see [`RESCUE_CLASSES`].
    pub recommended: bool,
}

/// Classes a boot medium actually needs: something has to see the disks and,
/// if the copies live on a share, the network. Everything else — display,
/// audio, printers, cameras — is weight the medium carries and never uses.
pub const RESCUE_CLASSES: [&str; 7] =
    ["SCSIAdapter", "HDC", "DiskDrive", "Volume", "Net", "USB", "System"];

/// Driver packages published on this machine, largest first.
///
/// Read through PowerShell's DISM cmdlet as JSON rather than by parsing
/// `pnputil`: pnputil's output is localised, and we would be matching Russian
/// captions on a Russian Windows and English ones elsewhere.
pub fn list_driver_packages() -> Vec<DriverPackage> {
    use std::os::windows::process::CommandExt;

    #[derive(serde::Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Raw {
        driver: Option<String>,
        original_file_name: Option<String>,
        class_name: Option<String>,
        provider_name: Option<String>,
        version: Option<String>,
    }

    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Get-WindowsDriver -Online | Select-Object Driver,OriginalFileName,ClassName,\
             ProviderName,Version | ConvertTo-Json -Compress",
        ])
        .creation_flags(0x0800_0000)
        .output();
    let Ok(out) = out else {
        warn!("could not enumerate drivers");
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    // A single driver comes back as an object rather than an array.
    let raws: Vec<Raw> = serde_json::from_str::<Vec<Raw>>(&text)
        .or_else(|_| serde_json::from_str::<Raw>(&text).map(|r| vec![r]))
        .unwrap_or_default();

    let mut list: Vec<DriverPackage> = raws
        .into_iter()
        .filter_map(|r| {
            let inf = r.driver?;
            let original = r.original_file_name.unwrap_or_default();
            let class_name = r.class_name.unwrap_or_default();
            // The package's real cost is its folder in the driver store.
            let size = std::path::Path::new(&original)
                .parent()
                .map(dir_size)
                .unwrap_or(0);
            let recommended = RESCUE_CLASSES.iter().any(|c| c.eq_ignore_ascii_case(&class_name));
            Some(DriverPackage {
                inf,
                original_name: std::path::Path::new(&original)
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or(original),
                class_name,
                provider: r.provider_name.unwrap_or_default(),
                version: r.version.unwrap_or_default(),
                size_bytes: size,
                recommended,
            })
        })
        .collect();
    list.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));
    info!(count = list.len(), "driver packages enumerated");
    list
}

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
    /// Published names (`oem12.inf`) of the driver packages to put in. Empty
    /// makes a smaller medium that may not see this machine's disks.
    ///
    /// A list rather than a flag because "all of them" is what made the first
    /// medium 2.2 GB: exporting the whole driver store brings the display,
    /// audio and printer packages along, and a rescue medium uses none of them.
    pub drivers: Vec<String>,
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

/// The build's stages, in order, as the wizard shows them.
///
/// A queue rather than one bar: the two DISM steps take minutes and report
/// nothing, so a single percentage would sit still and look hung. A list of
/// stages says which one is running even when that one cannot say how far along
/// it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildStage {
    Checking,
    OpeningImage,
    CopyingProgram,
    CopyingRuntime,
    AddingDrivers,
    SystemLibraries,
    WritingImage,
    BootFiles,
    WritingIso,
    CleaningUp,
}

impl BuildStage {
    pub fn id(&self) -> &'static str {
        match self {
            BuildStage::Checking => "check",
            BuildStage::OpeningImage => "open",
            BuildStage::CopyingProgram => "program",
            BuildStage::CopyingRuntime => "runtime",
            BuildStage::AddingDrivers => "drivers",
            BuildStage::SystemLibraries => "libs",
            BuildStage::WritingImage => "write",
            BuildStage::BootFiles => "boot",
            BuildStage::WritingIso => "iso",
            BuildStage::CleaningUp => "clean",
        }
    }
    pub fn title(&self) -> &'static str {
        match self {
            BuildStage::Checking => "Проверка исходных файлов",
            BuildStage::OpeningImage => "Открытие образа восстановления",
            BuildStage::CopyingProgram => "Копирование программы",
            BuildStage::CopyingRuntime => "Копирование WebView2",
            BuildStage::AddingDrivers => "Добавление драйверов",
            BuildStage::SystemLibraries => "Системные библиотеки",
            BuildStage::WritingImage => "Запись образа",
            BuildStage::BootFiles => "Загрузочные файлы",
            BuildStage::WritingIso => "Запись ISO",
            BuildStage::CleaningUp => "Уборка временных файлов",
        }
    }

    /// The stages a build with these options will actually run — the queue is
    /// shown before anything starts, so it must not list work that will be
    /// skipped.
    pub fn plan(with_drivers: bool) -> Vec<BuildStage> {
        let mut v = vec![
            BuildStage::Checking,
            BuildStage::OpeningImage,
            BuildStage::CopyingProgram,
            BuildStage::CopyingRuntime,
        ];
        if with_drivers {
            v.push(BuildStage::AddingDrivers);
        }
        v.extend([
            BuildStage::SystemLibraries,
            BuildStage::WritingImage,
            BuildStage::BootFiles,
            BuildStage::WritingIso,
            BuildStage::CleaningUp,
        ]);
        v
    }
}

/// Where a build is: which stage, and how far into it when that is knowable.
pub struct StageReport<'a> {
    pub stage: BuildStage,
    /// 0–100 within the stage. A stage that cannot measure itself reports 0 on
    /// entry and 100 on completion, and the wizard shows it as indeterminate.
    pub percent: u64,
    pub detail: &'a str,
}

/// Build the medium. `progress` is called whenever the stage or its percentage
/// changes; the caller turns that into the queue the user watches.
pub fn build_media(
    req: &MediaBuildRequest,
    mut progress: impl FnMut(StageReport),
) -> Result<MediaBuildReport> {
    let mut report =
        MediaBuildReport { media_dir: req.out_dir.join("media"), ..Default::default() };
    let mut at = |p: &mut dyn FnMut(StageReport), stage, percent, detail: &str| {
        info!(stage = stage_id(stage), percent, detail, "media build");
        p(StageReport { stage, percent, detail });
    };

    at(&mut progress, BuildStage::Checking, 0, "чтение образа");
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
    at(&mut progress, BuildStage::Checking, 100, "");

    let work = &req.work_dir;
    let tree = work.join("tree");
    let staged_wim = work.join("boot.wim");
    nuke_tree(&tree);
    std::fs::create_dir_all(&tree)?;
    std::fs::create_dir_all(&report.media_dir)?;

    at(&mut progress, BuildStage::OpeningImage, 0, "копия образа");
    std::fs::copy(&req.base_wim, &staged_wim)?;
    clear_readonly(&staged_wim);
    at(&mut progress, BuildStage::OpeningImage, 30, "монтирование");
    let mounted = open_image(&staged_wim, &tree)?;
    report.unpacked = !mounted;
    at(
        &mut progress,
        BuildStage::OpeningImage,
        100,
        if mounted { "смонтирован" } else { "распакован" },
    );

    at(&mut progress, BuildStage::CopyingProgram, 0, "");
    let app_dir = tree.join("Program Files").join("OwnBackup");
    std::fs::create_dir_all(&app_dir)?;
    for (i, exe) in layout::PROGRAM_FILES.iter().enumerate() {
        std::fs::copy(req.program_dir.join(exe), app_dir.join(exe))?;
        at(
            &mut progress,
            BuildStage::CopyingProgram,
            ((i + 1) * 100 / layout::PROGRAM_FILES.len()) as u64,
            exe,
        );
    }

    // The runtime goes on the medium's partition, not into the image: WinPE
    // holds the whole image in RAM, and 250 MB of browser is not needed there
    // until the interface starts.
    at(&mut progress, BuildStage::CopyingRuntime, 0, "около 550 МБ");
    let media_app = report.media_dir.join(layout::MEDIA_DIR);
    let runtime_dst = media_app.join("WebView2");
    std::fs::create_dir_all(&runtime_dst)?;
    copy_tree(&req.webview2_dir, &runtime_dst)?;
    std::fs::write(media_app.join(layout::MEDIA_MARKER), layout::MEDIA_MARKER_TEXT.as_bytes())?;
    at(&mut progress, BuildStage::CopyingRuntime, 80, "лишнее из рантайма");
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
    at(&mut progress, BuildStage::CopyingRuntime, 100, "");

    if !req.drivers.is_empty() {
        at(&mut progress, BuildStage::AddingDrivers, 0, "выгрузка из хранилища");
        match add_drivers(&tree, work, &req.drivers, req.extra_drivers.as_deref(), &mut |pct, d| {
            progress(StageReport { stage: BuildStage::AddingDrivers, percent: pct, detail: d })
        }) {
            Ok(n) => report.drivers_added = n,
            Err(e) => {
                warn!(error = %e, "drivers were not added");
                report.warnings.push(format!(
                    "Драйверы добавить не удалось: {e}. Носитель соберётся, но может не \
                     увидеть диски на другом оборудовании."
                ));
            }
        }
        at(&mut progress, BuildStage::AddingDrivers, 100, "");
    }

    at(&mut progress, BuildStage::SystemLibraries, 0, "");
    let sys32 = tree.join("Windows").join("System32");
    for (i, dll) in layout::SYSTEM_DLLS.iter().enumerate() {
        let dst = sys32.join(dll);
        if !dst.exists() {
            let src = system32().join(dll);
            if src.exists() {
                let _ = std::fs::copy(&src, &dst);
            }
        }
        at(
            &mut progress,
            BuildStage::SystemLibraries,
            ((i + 1) * 100 / layout::SYSTEM_DLLS.len()) as u64,
            dll,
        );
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
    set_scratch_space(&tree, mounted);

    at(
        &mut progress,
        BuildStage::WritingImage,
        0,
        if mounted { "фиксация изменений" } else { "упаковка (несколько минут)" },
    );
    close_image(&staged_wim, &tree, mounted)?;
    at(&mut progress, BuildStage::WritingImage, 100, "");

    at(&mut progress, BuildStage::BootFiles, 0, "");
    lay_out_media(&staged_wim, &report.media_dir)?;
    report.image_bytes = std::fs::metadata(report.media_dir.join("sources").join("boot.wim"))
        .map(|m| m.len())
        .unwrap_or(0);
    report.media_bytes = dir_size(&report.media_dir);
    at(&mut progress, BuildStage::BootFiles, 100, "");

    info!(
        image_mb = report.image_bytes / 1_048_576,
        media_mb = report.media_bytes / 1_048_576,
        drivers = report.drivers_added,
        unpacked = report.unpacked,
        "boot medium assembled"
    );
    Ok(report)
}

/// Remove everything the build left behind. Separate from the build so the
/// caller can run it after the ISO is written — and so a failed build can call
/// it too, which is how the first run left 3 GB in ProgramData.
pub fn clean_up(work_dir: &Path, out_dir: &Path) -> u64 {
    let freed = dir_size(work_dir) + dir_size(out_dir);
    nuke_tree(&work_dir.join("tree"));
    nuke_tree(work_dir);
    nuke_tree(out_dir);
    let left = dir_size(work_dir) + dir_size(out_dir);
    if left > 0 {
        warn!(bytes = left, "some temporary files could not be removed");
    }
    freed.saturating_sub(left)
}

fn stage_id(s: BuildStage) -> &'static str {
    s.id()
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

/// Export the chosen driver packages and add them to the image.
///
/// `pnputil /export-driver <inf>` takes one package at a time, which is exactly
/// what is wanted: exporting `*` brings the whole driver store — display, audio,
/// printers — and that is what made the first medium 2.2 GB.
fn add_drivers(
    tree: &Path,
    work: &Path,
    chosen: &[String],
    extra: Option<&Path>,
    progress: &mut dyn FnMut(u64, &str),
) -> Result<u32> {
    let export = work.join("drivers");
    let _ = std::fs::remove_dir_all(&export);
    std::fs::create_dir_all(&export)?;

    let mut exported = 0u32;
    for (i, inf) in chosen.iter().enumerate() {
        let dst = export.join(inf.trim_end_matches(".inf"));
        std::fs::create_dir_all(&dst)?;
        match run("pnputil", &["/export-driver", inf, &dst.display().to_string()]) {
            Ok(()) => exported += 1,
            // One driver refusing to export is not a reason to lose the rest.
            Err(e) => warn!(inf = %inf, error = %e, "driver package not exported"),
        }
        progress(((i + 1) * 70 / chosen.len().max(1)) as u64, inf);
    }

    progress(80, "установка в образ");
    dism(&[
        &format!("/Image:{}", tree.display()),
        "/Add-Driver",
        &format!("/Driver:{}", export.display()),
        "/Recurse",
    ])?;
    if let Some(dir) = extra.filter(|d| d.is_dir()) {
        progress(90, "дополнительная папка");
        if dism(&[
            &format!("/Image:{}", tree.display()),
            "/Add-Driver",
            &format!("/Driver:{}", dir.display()),
            "/Recurse",
        ])
        .is_ok()
        {
            exported += count_inf(dir);
        }
    }
    let _ = std::fs::remove_dir_all(&export);
    Ok(exported)
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

    // Backup mode first: robocopy /B opens through SeBackup/SeRestore, which
    // administrators hold, and those privileges override the ACL instead of
    // arguing with it. Mirroring an empty folder over the tree empties it —
    // taking ownership of a WinSxS tree file by file takes minutes and often
    // still fails, which is how 3 GB were left in ProgramData the first time.
    let empty = std::env::temp_dir().join("ownbackup-empty");
    let _ = std::fs::create_dir_all(&empty);
    let _ = run("robocopy", &[
        &empty.display().to_string(),
        &tree.display().to_string(),
        "/MIR", "/B", "/R:1", "/W:1", "/NFL", "/NDL", "/NJH", "/NJS", "/NP",
    ]);
    let _ = std::fs::remove_dir_all(&empty);
    if std::fs::remove_dir_all(tree).is_ok() || !tree.exists() {
        return;
    }

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
    // robocopy reports what it did in the exit code: 0-7 are all success, 8 and
    // above are failures. Treating non-zero as an error would call every
    // successful copy a failure.
    let ok = if program.eq_ignore_ascii_case("robocopy") {
        out.status.code().map(|c| c < 8).unwrap_or(false)
    } else {
        out.status.success()
    };
    if ok {
        return Ok(());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let tail: Vec<&str> = text.lines().rev().filter(|l| !l.trim().is_empty()).take(3).collect();
    Err(OwnBackupError::ImageFormat(format!(
        "{program} завершился с ошибкой: {}",
        tail.into_iter().rev().collect::<Vec<_>>().join(" ")
    )))
}
