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
use crate::media_layout::{rescue_role, RESCUE_CLASS_GUIDS};
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
    /// Bound to a present device of a class a rescue medium needs.
    pub recommended: bool,
    /// What that device is, in words: "контроллер дисков", "сетевой адаптер".
    pub role: String,
    /// The device's own name, which is what the user recognises — "Intel(R)
    /// Ethernet Connection I219-V" rather than "oem34.inf". Several devices
    /// on one package (two identical cards) are all named.
    pub device: String,
    /// Not a package at all: a present disk or network device running on a
    /// driver that ships with Windows (`stornvme.inf`, `net1ic64.inf`). There
    /// is nothing to export, so it is listed only to be SAID — and the build
    /// checks that the base image carries the same driver
    /// ([`check_inbox_drivers`]).
    pub inbox: bool,
}

/// A present device of a class the medium cares about, with the INF it runs on.
#[derive(serde::Deserialize)]
struct PresentDevice {
    inf: Option<String>,
    guid: Option<String>,
    bus: Option<String>,
    name: Option<String>,
}

/// The devices of this machine a medium cares about — the rescue classes — each
/// with the INF it is bound to.
///
/// `-PresentOnly` keeps a printer unplugged two years ago out of the answer,
/// and the instance id's prefix is the bus.
///
/// The class filter comes BEFORE the per-device property lookup, and that is
/// the whole cost of this call: `Get-PnpDeviceProperty` is one call per
/// device, and a desktop has several hundred devices against a handful of
/// storage and network ones. Asking all of them took seconds.
fn present_devices() -> std::result::Result<Vec<PresentDevice>, String> {
    let guid_list = RESCUE_CLASS_GUIDS
        .iter()
        .map(|g| format!("'{g}'"))
        .collect::<Vec<_>>()
        .join(",");
    ps_json(&format!(
        "$g=@({guid_list}); Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue | \
         Where-Object {{ $g -contains $_.ClassGuid }} | ForEach-Object {{ \
           $p = Get-PnpDeviceProperty -InstanceId $_.InstanceId \
                  -KeyName 'DEVPKEY_Device_DriverInfPath' -ErrorAction SilentlyContinue; \
           if ($p.Data) {{ [pscustomobject]@{{ inf = $p.Data; guid = $_.ClassGuid; \
             bus = $_.InstanceId.Split('\\')[0]; name = $_.FriendlyName }} }} \
         }} | ConvertTo-Json -Compress"
    ))
    .map_err(|e| format!("не удалось перечислить устройства этой машины: {e}"))
}

/// Driver packages this machine could put on a rescue medium.
///
/// Two questions, asked of the system rather than of the files on disk.
///
/// *What is installed* comes from the DISM cmdlet: every third-party package,
/// with its provider, version and the folder it occupies in the driver store —
/// which is where its real size is.
///
/// *What is actually in this machine* comes from the device list, restricted to
/// devices that are PRESENT. That second question is the one that matters:
/// parsing the INF files gets the class right but not the bus, and the bus is
/// what separates an Ethernet card from the VPN adapter sitting in the same
/// class beside it. Only a package bound to a present device of the right class,
/// on a bus that class can really live on, is recommended.
///
/// Everything installed is still returned, so the interface can show what was
/// left out and what it would have cost.
pub fn list_driver_packages() -> std::result::Result<Vec<DriverPackage>, String> {
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
    // The two questions are independent, and each costs a PowerShell start plus
    // a walk of something large, so they are asked at the same time.
    let installed_job = std::thread::spawn(|| -> std::result::Result<Vec<Raw>, String> {
        ps_json(
            "Get-WindowsDriver -Online | Select-Object Driver,OriginalFileName,ClassName,\
             ProviderName,Version | ConvertTo-Json -Compress",
        )
    });

    let present = present_devices()?;
    let installed: Vec<Raw> = installed_job
        .join()
        .map_err(|_| "поток опроса драйверов упал".to_string())?
        .map_err(|e| format!("не удалось перечислить установленные драйверы: {e}"))?;

    // inf -> (role, device names) for everything a medium would want. One
    // package can drive several present devices — two identical cards — and
    // every one is named.
    struct Wanted {
        role: String,
        names: Vec<String>,
    }
    let mut wanted: std::collections::HashMap<String, Wanted> = Default::default();
    for d in &present {
        let (Some(inf), Some(guid), Some(bus)) = (&d.inf, &d.guid, &d.bus) else { continue };
        let Some(role) = rescue_role(guid, bus) else { continue };
        let entry = wanted.entry(inf.to_ascii_lowercase()).or_insert_with(|| Wanted {
            role: role.to_string(),
            names: Vec::new(),
        });
        let name = d.name.clone().unwrap_or_default();
        if !name.is_empty() && !entry.names.contains(&name) {
            entry.names.push(name);
        }
    }

    let mut list: Vec<DriverPackage> = installed
        .into_iter()
        .filter_map(|r| {
            let inf = r.driver?;
            let original = r.original_file_name.unwrap_or_default();
            let size = std::path::Path::new(&original).parent().map(dir_size).unwrap_or(0);
            let hit = wanted.get(&inf.to_ascii_lowercase());
            Some(DriverPackage {
                original_name: std::path::Path::new(&original)
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or(original),
                class_name: r.class_name.unwrap_or_default(),
                provider: r.provider_name.unwrap_or_default(),
                version: r.version.unwrap_or_default(),
                size_bytes: size,
                recommended: hit.is_some(),
                role: hit.map(|w| w.role.clone()).unwrap_or_default(),
                device: hit.map(|w| w.names.join(", ")).unwrap_or_default(),
                inbox: false,
                inf,
            })
        })
        .collect();

    // Disks and network running on Windows' own drivers: no package to offer,
    // but "nothing found" would be the wrong thing to say about them.
    let mut inbox: Vec<DriverPackage> = wanted
        .iter()
        .filter(|(inf, _)| !layout::is_published_inf(inf))
        .map(|(inf, w)| DriverPackage {
            inf: inf.clone(),
            original_name: inf.clone(),
            class_name: String::new(),
            provider: "Windows".into(),
            version: String::new(),
            size_bytes: 0,
            recommended: false,
            inbox: true,
            role: w.role.clone(),
            device: w.names.join(", "),
        })
        .collect();
    inbox.sort_by(|a, b| a.role.cmp(&b.role).then(a.device.cmp(&b.device)));
    list.sort_by(|a, b| b.recommended.cmp(&a.recommended).then(b.size_bytes.cmp(&a.size_bytes)));

    // Drivers this machine does not have but a virtual machine booted from the
    // medium will need. They go first: the list is otherwise ordered by size,
    // and this one's size is not known until it is extracted.
    if let Some(iso) = crate::media_guest_drivers_win::vmware_tools_iso() {
        info!(iso = %iso.display(), "VMware guest tools found — offering their drivers");
        list.insert(
            0,
            DriverPackage {
                inf: crate::media_guest_drivers_win::VMWARE_TOOLS_ID.to_string(),
                original_name: iso.display().to_string(),
                class_name: "Display".into(),
                provider: "VMware, Inc.".into(),
                version: String::new(),
                // Unknown until the tools image is unpacked; the interface shows
                // a dash rather than a made-up number.
                size_bytes: 0,
                recommended: true,
                role: "экран и устройства виртуальной машины".into(),
                device: "VMware Tools (windows.iso)".into(),
                inbox: false,
            },
        );
    }
    list.extend(inbox);

    info!(
        total = list.len(),
        recommended = list.iter().filter(|d| d.recommended).count(),
        inbox = list.iter().filter(|d| d.inbox).count(),
        "driver packages enumerated"
    );
    Ok(list)
}

/// Run a PowerShell one-liner that prints JSON, and parse it. A single object
/// comes back unwrapped rather than as a one-element array, which is a
/// PowerShell habit rather than a mistake, so both shapes are accepted.
/// `Err` значит «спросить не удалось», а не «ничего нет» — разница видна
/// пользователю: пустой список в мастере читается как «драйверов не нашлось», и
/// на урезанной Windows без PowerShell это была бы тихая ложь.
fn ps_json<T: serde::de::DeserializeOwned>(script: &str) -> std::result::Result<Vec<T>, String> {
    use std::os::windows::process::CommandExt;
    // Redirected, PowerShell writes in the console's OEM code page, and device
    // names are localised: "Расширяемый хост-контроллер…" arrived as "������"
    // (2026-09-17). Asking for UTF-8 is what makes the bytes mean one thing.
    let script = format!("[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; {script}");
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(0x0800_0000)
        .output();
    let out = match out {
        Ok(o) => o,
        Err(e) => {
            warn!(error = %e, "powershell did not run");
            return Err(format!("PowerShell не запустился: {e}"));
        }
    };
    // UTF-8 first; the OEM page only if something still slipped through.
    let text = layout::console_text(&out.stdout);
    if text.trim().is_empty() {
        if !out.status.success() {
            let err = layout::console_text(&out.stderr);
            warn!(status = ?out.status.code(), "powershell returned nothing");
            return Err(format!(
                "PowerShell завершился с кодом {:?}: {}",
                out.status.code(),
                err.trim()
            ));
        }
        return Ok(Vec::new()); // спросили, ответ пустой — это законный ответ
    }
    serde_json::from_str::<Vec<T>>(&text)
        .or_else(|_| serde_json::from_str::<T>(&text).map(|one| vec![one]))
        .map_err(|e| format!("ответ PowerShell не разобрался: {e}"))
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
    /// Leave `cmd` as the medium's shell instead of our own desktop.
    ///
    /// The way back, and off by default: a medium is built with our desktop as
    /// its shell unless this is asked for.
    ///
    /// It exists because with our desktop as the shell there is no console
    /// behind it, so a desktop that fails to start on some machine leaves a
    /// medium that does not come up at all. Two things answer that: the desktop
    /// itself falls back to `cmd.exe` rather than exiting, and this — one tick
    /// in the wizard and the medium behaves exactly as every medium before it.
    pub classic_console: bool,
    /// `%APPDATA%` of the person the medium is being built for.
    ///
    /// Not read from this process's environment, and that is the whole point:
    /// a build started from the interface runs inside the service, which is
    /// LocalSystem. Its `%APPDATA%` is the SYSTEM profile, where no third-party
    /// program has ever kept a setting — so the tools went onto the medium
    /// freshly installed while the build reported success.
    pub user_appdata: Option<PathBuf>,
    /// Ids from [`own_backup_common::media_tools::MEDIA_TOOLS`] the user ticked.
    ///
    /// Asked for by id rather than by path so the build looks them up here, on
    /// the machine that has them, and a request cannot name a folder that was
    /// never offered.
    pub tools: Vec<String>,
    /// Put the WebView2 runtime inside `boot.wim` rather than beside it.
    ///
    /// A real trade, not a preference. Inside, it is compressed with the rest of
    /// the image — LZX gets about four to one, so ~400 MB of runtime costs ~110
    /// MB of medium. But WinPE loads the whole of `boot.wim` into a RAM disk
    /// before anything runs, so that compressed copy occupies memory from boot
    /// to shutdown whether the interface is ever opened or not. Outside, the
    /// files are read from the medium as they are needed and cost no memory, at
    /// the price of their full size on the ISO.
    ///
    /// The launcher handles both: it looks for the marker file on the medium
    /// first and falls back to the path inside the image.
    pub runtime_in_image: bool,
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
    /// Third-party tools copied in, and what they cost.
    pub tools_added: u32,
    pub tools_bytes: u64,
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
    CopyingTools,
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
            BuildStage::CopyingTools => "tools",
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
            BuildStage::CopyingTools => "Копирование сторонних средств",
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
    pub fn plan(with_drivers: bool, with_tools: bool) -> Vec<BuildStage> {
        let mut v = vec![
            BuildStage::Checking,
            BuildStage::OpeningImage,
            BuildStage::CopyingProgram,
        ];
        if with_tools {
            v.push(BuildStage::CopyingTools);
        }
        v.push(BuildStage::CopyingRuntime);
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
    // Mounting is quick; unpacking is minutes, and its own percentage is mapped
    // into what is left of this stage's bar rather than restarting it at zero.
    let mounted = open_image(&staged_wim, &tree, &mut |pct| {
        progress(StageReport {
            stage: BuildStage::OpeningImage,
            percent: 30 + pct * 70 / 100,
            detail: "распаковка",
        });
    })?;
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

    if !req.tools.is_empty() {
        at(&mut progress, BuildStage::CopyingTools, 0, "");
        match add_tools(&tree, &req.tools, req.user_appdata.as_deref(), &mut |pct, what| {
            at(&mut progress, BuildStage::CopyingTools, pct, what);
        }) {
            Ok((count, bytes)) => {
                report.tools_added = count;
                report.tools_bytes = bytes;
            }
            // Never fatal. A medium without Total Commander is the medium we
            // shipped yesterday; a build that failed at the last convenience is
            // an hour of someone's evening.
            Err(e) => {
                warn!(error = %e, "third-party tools were not added");
                report.warnings.push(format!("Сторонние средства не добавлены: {e}"));
            }
        }
        at(&mut progress, BuildStage::CopyingTools, 100, "");
    }

    at(
        &mut progress,
        BuildStage::CopyingRuntime,
        0,
        if req.runtime_in_image { "в образ, со сжатием" } else { "на носитель, без сжатия" },
    );
    // Every medium carries the marker: WinPE assigns its volume whatever letter
    // it likes, and the launcher finds it by this file — for a runtime kept
    // outside the image, and for the copy of the logs it keeps there.
    let media_app = report.media_dir.join(layout::MEDIA_DIR);
    std::fs::create_dir_all(&media_app)?;
    std::fs::write(media_app.join(layout::MEDIA_MARKER), layout::MEDIA_MARKER_TEXT.as_bytes())?;
    let runtime_dst = if req.runtime_in_image {
        app_dir.join("WebView2")
    } else {
        media_app.join("WebView2")
    };
    std::fs::create_dir_all(&runtime_dst)?;
    copy_tree(&req.webview2_dir, &runtime_dst)?;
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

    // The chosen list mixes real packages from this machine's driver store with
    // pseudo-packages that have to be fetched from somewhere else (a
    // hypervisor's guest tools). They are separated here, before anything hands
    // an id that is not an INF name to `pnputil`.
    let guest_prefix = crate::media_guest_drivers_win::PSEUDO_PREFIX;
    let (guest, store): (Vec<String>, Vec<String>) =
        req.drivers.iter().cloned().partition(|d| d.starts_with(guest_prefix));

    if !store.is_empty() || !guest.is_empty() {
        at(&mut progress, BuildStage::AddingDrivers, 0, "выгрузка из хранилища");
        if !store.is_empty() {
            match add_drivers(&tree, work, &store, req.extra_drivers.as_deref(), &mut |pct, d| {
                progress(StageReport { stage: BuildStage::AddingDrivers, percent: pct, detail: d })
            }) {
                Ok(n) => report.drivers_added = n,
                Err(e) => {
                    warn!(error = %e, "drivers were not added");
                    report.warnings.push(format!(
                        "Драйверы добавить не удалось: {}. Носитель соберётся, но может не \
                         увидеть диски на другом оборудовании.",
                        user_reason(&e)
                    ));
                }
            }
        }
        if guest.iter().any(|d| d == crate::media_guest_drivers_win::VMWARE_TOOLS_ID) {
            match add_guest_vmware(&tree, work, &mut |pct, d| {
                progress(StageReport { stage: BuildStage::AddingDrivers, percent: pct, detail: d })
            }) {
                Ok((n, notes)) => {
                    report.drivers_added += n;
                    report.warnings.extend(notes);
                }
                Err(e) => {
                    warn!(error = %e, "VMware guest drivers were not added");
                    report.warnings.push(format!(
                        "Драйверы VMware Tools добавить не удалось: {}. Носитель соберётся, \
                         но в виртуальной машине VMware останется разрешение 1024×768.",
                        user_reason(&e)
                    ));
                }
            }
        }
        at(&mut progress, BuildStage::AddingDrivers, 100, "");
    }
    check_inbox_drivers(&tree, &mut report);

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

    // Windows' own iSCSI initiator: without its user-mode files the medium
    // cannot mount a backup at all, and the failure would only show much
    // later, in front of someone trying to restore.
    let mut missing_iscsi = Vec::new();
    for name in layout::ISCSI_FILES {
        let dst = sys32.join(name);
        if dst.exists() {
            continue;
        }
        let src = system32().join(name);
        if src.exists() {
            let _ = std::fs::copy(&src, &dst);
        } else {
            missing_iscsi.push(name);
        }
    }
    if !missing_iscsi.is_empty() {
        warn!(files = ?missing_iscsi, "media: iSCSI initiator files missing on the build host");
        report.warnings.push(format!(
            "На этой машине не нашлось файлов iSCSI-инициатора ({}). Носитель соберётся, \
             но подключить копию как диск на нём не выйдет.",
            missing_iscsi.join(", ")
        ));
    }

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
    // То, что человек увидит, если интерфейса не будет: рабочий стол запускает
    // start-media.cmd без окна, и напечатанное там на экран не попадает.
    std::fs::write(
        app_dir.join("no-interface.cmd"),
        layout::no_interface_cmd(app_dir_win),
    )?;
    // WinRE ships its own winpeshl.ini, which launches the recovery menu. It
    // must never survive: either ours replaces it, or there must be none at all
    // so WinPE falls back to `cmd /k startnet.cmd`.
    let _ = std::fs::remove_file(sys32.join("winpeshl.ini"));
    if !req.classic_console {
        std::fs::write(sys32.join("winpeshl.ini"), layout::winpeshl_ini(app_dir_win))?;
    }
    // Exactly one of the two starts the desktop — see `startnet_cmd`.
    std::fs::write(
        sys32.join("startnet.cmd"),
        layout::startnet_cmd(app_dir_win, req.classic_console),
    )?;
    info!(
        shell = if req.classic_console { "cmd" } else { "own-backup-shell" },
        "the medium's shell"
    );
    set_scratch_space(&tree, mounted);
    set_input_locales(&tree, mounted, &mut report);

    at(
        &mut progress,
        BuildStage::WritingImage,
        0,
        if mounted { "фиксация изменений" } else { "упаковка (несколько минут)" },
    );
    let writing_detail = if mounted { "фиксация изменений" } else { "упаковка" };
    close_image(&staged_wim, &tree, mounted, &mut |pct| {
        progress(StageReport {
            stage: BuildStage::WritingImage,
            percent: pct,
            detail: writing_detail,
        });
    })?;
    at(&mut progress, BuildStage::WritingImage, 100, "");

    at(&mut progress, BuildStage::BootFiles, 0, "");
    if let Some(missing) = lay_out_media(&staged_wim, &report.media_dir)? {
        report.warnings.push(missing);
    }
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
    clean_up_with_progress(work_dir, out_dir, &mut |_, _| {})
}

/// [`clean_up`], saying which part of it is running.
///
/// Not one act but four, and each of them minutes: measuring several gigabytes
/// of unpacked image, then removing the tree, the work folder and the staging
/// folder — the removals through robocopy when the image's ACLs refuse a plain
/// delete. The step used to report nothing at all until it was over.
pub fn clean_up_with_progress(
    work_dir: &Path,
    out_dir: &Path,
    progress: &mut dyn FnMut(u64, &str),
) -> u64 {
    progress(0, "подсчёт");
    let freed = dir_size(work_dir) + dir_size(out_dir);
    progress(10, "распакованный образ");
    nuke_tree(&work_dir.join("tree"));
    progress(45, "рабочая папка");
    nuke_tree(work_dir);
    progress(75, "файлы носителя");
    nuke_tree(out_dir);
    progress(95, "проверка");
    let left = dir_size(work_dir) + dir_size(out_dir);
    if left > 0 {
        warn!(bytes = left, "some temporary files could not be removed");
    }
    progress(100, "");
    freed.saturating_sub(left)
}

fn stage_id(s: BuildStage) -> &'static str {
    s.id()
}

/// Mount the image; fall back to unpacking it. Returns true when mounted.
///
/// `progress` reports 0–100 of whichever operation is running — mounting is
/// quick, unpacking is minutes, and only DISM knows how far along it is.
fn open_image(wim: &Path, tree: &Path, progress: &mut dyn FnMut(u64)) -> Result<bool> {
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
    dism_progress(
        &["/Apply-Image", &format!("/ImageFile:{}", wim.display()), "/Index:1",
          &format!("/ApplyDir:{}", tree.display())],
        progress,
    )?;
    Ok(false)
}

fn close_image(
    wim: &Path,
    tree: &Path,
    mounted: bool,
    progress: &mut dyn FnMut(u64),
) -> Result<()> {
    if mounted {
        // Committing a mounted image writes back everything that changed —
        // the program, the runtime, the drivers. Minutes, and DISM counts them.
        return dism_progress(
            &["/Unmount-Wim", &format!("/MountDir:{}", tree.display()), "/Commit"],
            progress,
        );
    }
    let _ = std::fs::remove_file(wim);
    dism_progress(
        &[
            "/Capture-Image",
            &format!("/ImageFile:{}", wim.display()),
            &format!("/CaptureDir:{}", tree.display()),
            "/Name:My Own Backup media",
            "/Compress:max",
            "/Bootable",
        ],
        progress,
    )
}

/// The part of an error a person should read. The variant's English prefix
/// ("Image format error: ") belongs in the log, not in a line shown in a dialog
/// under a red border.
fn user_reason(e: &OwnBackupError) -> String {
    let text = e.to_string();
    for prefix in ["Image format error: ", "I/O error: ", "Device I/O error: "] {
        if let Some(rest) = text.strip_prefix(prefix) {
            return rest.to_string();
        }
    }
    text
}

/// Take the guest drivers out of VMware Tools and put them in the image.
///
/// Kept here rather than in the guest-drivers module so that module never has
/// to know how this image is being serviced: it stages files, DISM is ours.
fn add_guest_vmware(
    tree: &Path,
    work: &Path,
    progress: &mut dyn FnMut(u64, &str),
) -> Result<(u32, Vec<String>)> {
    let iso = crate::media_guest_drivers_win::vmware_tools_iso().ok_or_else(|| {
        OwnBackupError::ImageFormat(
            "windows.iso (VMware Tools) больше не найден — драйверы VMware пропущены".into(),
        )
    })?;
    crate::media_guest_drivers_win::add_vmware_tools_drivers(
        &iso,
        work,
        |pct, detail| progress(pct, detail),
        |staged| {
            dism(&[
                &format!("/Image:{}", tree.display()),
                "/Add-Driver",
                &format!("/Driver:{}", staged.display()),
                "/Recurse",
            ])
        },
    )
}

/// Keyboard layouts in the image (see [`layout::INPUT_LOCALES`]).
///
/// Two levers, because neither covers both cases: DISM writes the layouts into
/// the image's registry, which only works while the image is mounted, and the
/// boot script asks `wpeutil` for them at every start, which is all an unpacked
/// image has. Both are best-effort — a medium that boots with one layout is
/// still a medium; one that fails to build is not.
fn set_input_locales(tree: &Path, mounted: bool, report: &mut MediaBuildReport) {
    // A layout whose DLL is absent silently does not switch, so fill in what
    // the image lacks from the machine doing the building.
    let sys32 = tree.join("Windows").join("System32");
    for dll in layout::KEYBOARD_DLLS {
        let dst = sys32.join(dll);
        if dst.exists() {
            continue;
        }
        let src = system32().join(dll);
        if src.exists() {
            let _ = std::fs::copy(&src, &dst);
        } else {
            warn!(dll, "keyboard layout DLL is in neither the image nor this machine");
            report.warnings.push(format!(
                "{dll} нет ни в образе, ни на этой машине — соответствующая раскладка \
                 клавиатуры на носителе может не переключаться."
            ));
        }
    }
    if mounted {
        if let Err(e) = dism(&[
            &format!("/Image:{}", tree.display()),
            &format!("/Set-InputLocale:{}", layout::INPUT_LOCALES),
        ]) {
            warn!(error = %e, "input locales were not written into the image");
            report.warnings.push(format!(
                "Раскладки клавиатуры не записались в образ ({e}). Носитель загрузится, \
                 раскладки выставит скрипт запуска (Alt+Shift)."
            ));
        }
    }
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

/// Make sure the disks and network of THIS machine work on the medium when
/// they run on Windows' own drivers.
///
/// Nothing is exported for those, which used to leave the question unasked:
/// the recovery environment carries a subset of Windows' drivers, not all of
/// them, and "the full Windows has it" says nothing about WinRE. So each INF is
/// looked for in the opened image; one that is missing is added from this
/// machine's driver store, and one that cannot be added is reported by the
/// device's name — a medium that boots and then sees no disk should never be
/// the first news of it.
fn check_inbox_drivers(tree: &Path, report: &mut MediaBuildReport) {
    let devices = match present_devices() {
        Ok(d) => d,
        Err(e) => {
            warn!(error = %e, "inbox drivers not checked");
            report.warnings.push(format!(
                "Не удалось проверить, есть ли в образе встроенные драйверы дисков и сети \
                 этой машины: {e}"
            ));
            return;
        }
    };
    let mut seen = std::collections::BTreeSet::new();
    for d in &devices {
        let (Some(inf), Some(guid), Some(bus)) = (&d.inf, &d.guid, &d.bus) else { continue };
        let inf = inf.to_ascii_lowercase();
        if layout::is_published_inf(&inf) || rescue_role(guid, bus).is_none() || !seen.insert(inf.clone())
        {
            continue;
        }
        let name = d.name.clone().unwrap_or_else(|| inf.clone());
        if tree.join("Windows").join("INF").join(&inf).exists() {
            info!(%inf, device = %name, "inbox driver is in the base image");
            continue;
        }
        let Some(pkg) = host_inbox_package(&inf) else {
            warn!(%inf, device = %name, "inbox driver missing from the base image and from the driver store");
            report.warnings.push(format!(
                "{name}: драйвера {inf} нет в среде восстановления, и в хранилище драйверов \
                 этой машины его не нашлось — на носителе это устройство может не работать."
            ));
            continue;
        };
        match dism(&[
            &format!("/Image:{}", tree.display()),
            "/Add-Driver",
            &format!("/Driver:{}", pkg.display()),
        ]) {
            Ok(()) => {
                info!(%inf, device = %name, package = %pkg.display(), "inbox driver added to the image");
                report.drivers_added += 1;
            }
            Err(e) => {
                warn!(%inf, device = %name, error = %e, "inbox driver could not be added");
                report.warnings.push(format!(
                    "{name}: драйвера {inf} нет в среде восстановления, и добавить его с этой \
                     машины не удалось ({}) — на носителе это устройство может не работать.",
                    user_reason(&e)
                ));
            }
        }
    }
}

/// This machine's copy of an inbox driver package:
/// `DriverStore\FileRepository\<inf>_amd64_<hash>\<inf>`. Several versions can
/// sit side by side after updates; the newest folder is the one in use.
fn host_inbox_package(inf: &str) -> Option<PathBuf> {
    let repo = system32().join("DriverStore").join("FileRepository");
    let prefix = format!("{inf}_");
    std::fs::read_dir(&repo)
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().to_ascii_lowercase().starts_with(&prefix))
        .map(|e| e.path().join(inf))
        .filter(|p| p.exists())
        .max_by_key(|p| p.parent().and_then(|d| d.metadata().ok()).and_then(|m| m.modified().ok()))
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
/// `\sources\boot.wim` off a ramdisk. No oscdimg, no ADK.
/// Returns a warning for the person building the medium when the boot chain
/// came out incomplete — `Ok(None)` means it is whole.
fn lay_out_media(wim: &Path, media: &Path) -> Result<Option<String>> {
    let boot = PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()))
        .join("Boot");
    std::fs::create_dir_all(media.join(r"EFI\BOOT"))?;
    std::fs::create_dir_all(media.join(r"EFI\Microsoft\Boot"))?;
    std::fs::create_dir_all(media.join("boot"))?;
    std::fs::create_dir_all(media.join("sources"))?;
    std::fs::copy(boot.join(r"EFI\bootmgfw.efi"), media.join(r"EFI\BOOT\BOOTX64.EFI"))?;
    let bcd = media.join(r"EFI\Microsoft\Boot\BCD");
    std::fs::copy(boot.join(r"DVD\EFI\BCD"), &bcd)?;
    clear_readonly(&bcd);
    // `boot.stl` — the certificate trust list, and the ONE file that decides
    // whether this medium boots on a machine with Secure Boot on. Without it
    // the loader starts, reads its BCD and stops at `0xc000000f` — which is
    // `STATUS_NO_SUCH_FILE`, a file it could not find and does not name.
    // With Secure Boot off the same medium boots, which is what sent the first
    // diagnosis chasing the ISO's filesystem instead.
    //
    // Found by bisection on a real medium, 2026-09-20: eleven files Windows
    // keeps beside its loader were added, the medium booted, and they were
    // taken away in groups until one was left. Not `winsipolicy.p7b`, not the
    // CI policies, not a second copy of the loader — `boot.stl` alone, and
    // without it the same medium fails again.
    //
    // Taken from the running Windows, like the loader beside it, so the two
    // always come from one servicing level. A machine old enough not to have
    // it builds a medium that boots with Secure Boot off — better than
    // refusing to build one at all.
    let stl = boot.join(r"EFI\boot.stl");
    let mut missing_stl = None;
    if stl.is_file() {
        let dst = media.join(r"EFI\Microsoft\Boot\boot.stl");
        std::fs::copy(&stl, &dst)?;
        clear_readonly(&dst);
    } else {
        warn!(
            path = %stl.display(),
            "boot.stl not found — the medium will not boot with Secure Boot enabled"
        );
        missing_stl = Some(format!(
            "На этой машине нет файла {} — носитель собран без него и НЕ ЗАГРУЗИТСЯ, \
             пока в прошивке включена безопасная загрузка. Её придётся выключить.",
            stl.display()
        ));
    }
    // The medium carries no display driver, so the screen is Microsoft Basic
    // Display, and on UEFI that driver draws on the framebuffer the boot
    // loader left behind — it cannot switch modes afterwards
    // (ChangeDisplaySettingsEx → -1). The resolution has to be chosen BEFORE
    // Windows starts, and this is where: the loader is told to take the
    // highest mode the firmware offers instead of its 1024×768 default. A
    // failure here costs a small screen, not the medium, so it only warns.
    //
    // "The highest the firmware offers" depends on the firmware's SETUP, not
    // on anything on the medium (2026-09-18, GeForce GTX 1070 Ti, Samsung
    // C49G95T). With CSM — the Compatibility Support Module — enabled, the top
    // of the list was 1280×1024, by all appearances the modes of the card's
    // legacy video BIOS; with CSM disabled the card's own UEFI driver answers,
    // and the medium came up in the monitor's recommended mode. That is also why
    // video adapter drivers are no longer offered for the medium: the GeForce
    // one never gave a picture in WinPE (four outputs enumerated, none lit), and
    // the firmware gives the right mode by itself once asked properly.
    if let Err(e) =
        run("bcdedit", &["/store", &bcd.display().to_string(), "/set", "{default}", "highestmode", "on"])
    {
        warn!(error = %e, "highestmode not set — the medium boots at the loader's default resolution");
    }
    // bcdedit opens the store as a registry hive and leaves its transaction
    // logs behind. Once it has exited they are empty and mean nothing; the
    // shipped store has none. A non-empty one is left alone.
    for log in ["BCD.LOG", "BCD.LOG1", "BCD.LOG2"] {
        let p = media.join(r"EFI\Microsoft\Boot").join(log);
        if std::fs::metadata(&p).is_ok_and(|m| m.len() == 0) {
            let _ = std::fs::remove_file(&p);
        }
    }
    std::fs::copy(boot.join(r"DVD\EFI\boot.sdi"), media.join(r"boot\boot.sdi"))?;
    let dst = media.join(r"sources\boot.wim");
    let _ = std::fs::remove_file(&dst);
    std::fs::rename(wim, &dst).or_else(|_| std::fs::copy(wim, &dst).map(|_| ()))?;
    Ok(missing_stl)
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

/// Put the chosen third-party tools into the image.
///
/// Straight under `Program Files`, beside our own folder: the desktop looks
/// for them exactly there
/// ([`own_backup_common::media_tools::MediaTool::path_on_medium`]), and that is
/// where anyone would look for them on any Windows.
///
/// Whole folders, settings and all. They are copied as they are on the machine
/// making the medium, which is the point — a Total Commander with the user's
/// own panels and hotkeys is worth more in a rescue than a default one. That
/// those settings do not survive a reboot is fine and expected: `X:` is a RAM
/// disk, and the next boot brings the same copy back.
fn add_tools(
    tree: &Path,
    wanted: &[String],
    user_appdata: Option<&Path>,
    progress: &mut dyn FnMut(u64, &str),
) -> Result<(u32, u64)> {
    let found = crate::media_tools_win::find_tools();
    let picked: Vec<&crate::media_tools_win::FoundTool> =
        found.iter().filter(|f| wanted.iter().any(|id| id == &f.id)).collect();
    if picked.len() < wanted.len() {
        // Asked for something this machine does not have. Worth saying out
        // loud: the wizard offered it a moment ago.
        let missing: Vec<&str> = wanted
            .iter()
            .map(String::as_str)
            .filter(|id| !picked.iter().any(|f| f.id == *id))
            .collect();
        warn!(missing = ?missing, "tools asked for but not found on this machine");
    }

    let root = tree.join("Program Files");
    let mut count = 0;
    let mut bytes = 0;
    for (n, tool) in picked.iter().enumerate() {
        let Some(known) = own_backup_common::media_tools::tool(&tool.id) else { continue };
        let to = root.join(known.folder);
        progress(((n * 100) / picked.len().max(1)) as u64, &tool.name);
        std::fs::create_dir_all(&to)?;
        copy_tree_except(&tool.source, &to, known.skip)?;
        if !to.join(known.exe).is_file() {
            return Err(OwnBackupError::ImageFormat(format!(
                "{} скопирован, но {} в нём нет",
                tool.name, known.exe
            )));
        }
        // Counted, not assumed. A program that is nearly all there starts and
        // then fails at something specific — and the failure it reports is
        // about whatever it could not find, never about the copy. Cheaper to
        // catch here than on a booted medium.
        let (copied_bytes, copied_files) = crate::media_tools_win::weigh(&to);
        if copied_files < tool.files || copied_bytes < tool.size_bytes {
            return Err(OwnBackupError::ImageFormat(format!(
                "{} скопирован не полностью: {} файлов вместо {} ({} вместо {} байт)",
                tool.name, copied_files, tool.files, copied_bytes, tool.size_bytes
            )));
        }

        // The settings the person actually uses, which live nowhere near the
        // program — see `MediaTool::settings_in_appdata`.
        if let Some(under) = known.settings_in_appdata {
            match copy_settings(under, &to, user_appdata) {
                0 => warn!(
                    tool = %tool.id,
                    from = under,
                    "no settings found - the tool goes on the medium as freshly installed"
                ),
                taken => {
                    info!(tool = %tool.id, from = under, files = taken, "the tool's own settings copied")
                }
            }
        }

        // And the file that makes it read them from there — see
        // `MediaTool::portable_marker`. After the settings, so a program that
        // somehow starts mid-build never sees the marker without them.
        if let Some(marker) = known.portable_marker {
            std::fs::write(to.join(marker), b"")?;
            info!(tool = %tool.id, marker, "the tool is told to keep its settings beside itself");
        }

        info!(
            tool = %tool.id,
            from = %tool.source.display(),
            mb = tool.size_bytes / 1_048_576,
            files = copied_files,
            "third-party tool put on the medium"
        );
        count += 1;
        bytes += tool.size_bytes;
    }
    Ok((count, bytes))
}

/// Copy a program's settings out of `%APPDATA%\<under>` and in beside the
/// program on the medium.
///
/// **The top-level files, and only those.** That is the settings and nothing
/// else: `config.xml`, `stylers.xml`, `wincmd.ini`, and the toolbar files
/// (`.bar`, `.br2`) a Total Commander toolbar is useless without. What lives in
/// the SUBfolders is caches, plugin state and — in Notepad++'s case — the backup
/// copies of documents a person never saved; putting someone's unsaved files
/// on a medium they may hand to somebody else is not a mistake worth making
/// once.
///
/// One exception, and it is not about privacy: `session.xml` is the list of
/// files that were open, all of them on the machine's own disk. Restoring it on
/// a rescue medium would have Notepad++ reach for the very disk the medium was
/// booted to save, before anybody asked it to.
///
/// Never fatal: a machine where the program has never been started has no such
/// folder, and a medium carrying default settings is still a working medium.
fn copy_settings(under: &str, into: &Path, user_appdata: Option<&Path>) -> u32 {
    let own;
    let appdata = match user_appdata {
        Some(given) => given,
        // Only right when the build runs in somebody's own session. Inside the
        // service it is the SYSTEM profile and holds nothing.
        None => match std::env::var("APPDATA") {
            Ok(from_env) => {
                own = PathBuf::from(from_env);
                own.as_path()
            }
            Err(_) => return 0,
        },
    };
    let from = appdata.join(under);
    let Ok(entries) = std::fs::read_dir(&from) else { return 0 };
    let mut taken = 0;
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name();
        if name.to_string_lossy().to_ascii_lowercase().starts_with("session.") {
            continue;
        }
        if std::fs::copy(entry.path(), into.join(&name)).is_ok() {
            taken += 1;
        }
    }
    taken
}

fn system32() -> PathBuf {
    PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()))
        .join("System32")
}

/// [`copy_tree`], minus the top-level folders named in `skip`.
///
/// Only the top level, and deliberately: a name like `themes` could appear
/// again somewhere deep inside a program's own data, and leaving THAT out is
/// not what anybody asked for.
fn copy_tree_except(src: &Path, dst: &Path, skip: &[&str]) -> Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if skip.iter().any(|s| s.eq_ignore_ascii_case(&name.to_string_lossy())) {
            info!(folder = %name.to_string_lossy(), "left behind, nothing on a medium reads it");
            continue;
        }
        let to = dst.join(&name);
        if entry.file_type()?.is_dir() {
            std::fs::create_dir_all(&to)?;
            copy_tree(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
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

/// DISM, reporting the percentage it prints as it prints it.
///
/// The two long steps — unpacking an image and packing it back — take minutes,
/// and DISM does say how far along it is: it draws
/// `[==========          ]  50.0%` on stdout and redraws that same line with a
/// carriage return. Collecting the output with `.output()` hands all of it over
/// at the end, which is exactly why those stages looked frozen. Reading the
/// pipe while it fills turns them into a bar that moves.
///
/// Failure is reported like [`run`]: with the last lines DISM printed, since
/// its own message is the useful part.
fn dism_progress(args: &[&str], progress: &mut dyn FnMut(u64)) -> Result<()> {
    use std::io::Read;
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let mut child = std::process::Command::new("dism")
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| OwnBackupError::ImageFormat(format!("dism: {e}")))?;

    let mut out = child.stdout.take();
    // The tail is kept for the error message; the whole output of an unpack is
    // thousands of redraws and none of it is worth holding.
    let mut tail = String::new();
    let mut last = u64::MAX;
    if let Some(pipe) = out.as_mut() {
        let mut buf = [0u8; 4096];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let chunk = layout::console_text(&buf[..n]);
                    if let Some(pct) = layout::dism_percent(&chunk) {
                        if pct != last {
                            last = pct;
                            progress(pct);
                        }
                    }
                    tail.push_str(&chunk);
                    if tail.len() > 8192 {
                        // Keep the end: that is where the error lands.
                        tail = tail.split_off(tail.len() - 4096);
                    }
                }
            }
        }
    }

    let status = child
        .wait()
        .map_err(|e| OwnBackupError::ImageFormat(format!("dism: {e}")))?;
    if status.success() {
        return Ok(());
    }
    let lines: Vec<&str> = tail
        .split(['\r', '\n'])
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('['))
        .rev()
        .take(3)
        .collect();
    Err(OwnBackupError::ImageFormat(format!(
        "dism завершился с ошибкой: {}",
        lines.into_iter().rev().collect::<Vec<_>>().join(" ")
    )))
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
    let text = layout::console_text(&out.stdout);
    let tail: Vec<&str> = text.lines().rev().filter(|l| !l.trim().is_empty()).take(3).collect();
    Err(OwnBackupError::ImageFormat(format!(
        "{program} завершился с ошибкой: {}",
        tail.into_iter().rev().collect::<Vec<_>>().join(" ")
    )))
}


