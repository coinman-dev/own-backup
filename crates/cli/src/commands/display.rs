//! Pick a screen mode worth showing an interface on.
//!
//! Boot media comes up in whatever the firmware left behind — 1024×768 on a
//! VMware guest, 800×600 on some real machines — and an interface built for a
//! desktop has nowhere to put itself. Windows itself does not choose for us:
//! WinPE has no display settings, no monitor profile and no user to ask.
//!
//! On real hardware the choice is the one a person would make: ask the display
//! driver what it can do and take the largest true-colour mode, which on a
//! monitor that reports its EDID is the monitor's own preferred one.
//!
//! **In a virtual machine that reasoning does not hold.** The guest driver's
//! list is a catalogue of what it can be *told* to do — a VMware guest offers
//! twenty-eight modes up to 3840×2400 — and not one of them describes the
//! window the guest is actually displayed in. Picking the biggest there fills
//! the host's screen with a window nobody can use; worse on a 49-inch
//! ultra-wide, where the host's own screen is not that shape either. So in a
//! virtual machine a size that fits anywhere is aimed for instead of the
//! maximum, and `--biggest` is there for whoever disagrees.

use clap::Args;

#[derive(Args)]
pub struct DisplayArgs {
    /// Smallest acceptable mode, e.g. 1280x720. Nothing is changed when the
    /// screen already meets it and no better mode exists.
    #[arg(long, default_value = "1280x720")]
    pub min: String,

    /// The size to aim for in a virtual machine, where the mode list says
    /// nothing about the window the guest is shown in.
    #[arg(long, default_value = "1440x900")]
    pub prefer: String,

    /// Take the largest mode on offer even in a virtual machine.
    #[arg(long)]
    pub biggest: bool,

    /// Show what the display can do and change nothing.
    #[arg(long)]
    pub list: bool,

    /// Why is the resolution what it is: which adapter drives the screen, which
    /// driver packages are in the image, and what PnP said about them.
    #[arg(long)]
    pub diag: bool,
}

/// One screen mode.
///
/// Plain data, and outside the Windows-only code on purpose: choosing between
/// modes is the part worth testing, and it tests on any machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub bpp: u32,
    pub hz: u32,
}

impl Mode {
    fn area(&self) -> u64 {
        self.width as u64 * self.height as u64
    }
    /// True colour and big enough to show something in.
    fn usable(&self) -> bool {
        self.bpp >= 32 && self.width >= 640 && self.height >= 480
    }
    /// A refresh rate a monitor would accept; 1 Hz means "unspecified".
    fn sane_hz(&self) -> bool {
        self.hz <= 75
    }
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
impl DisplayArgs {
    fn sizes(&self) -> Option<((u32, u32), (u32, u32))> {
        Some((parse_size(&self.min)?, parse_size(&self.prefer)?))
    }
}

/// Which virtual machine this display belongs to, if any.
///
/// By the adapter's PCI id first — a vendor id does not get renamed — and by
/// its name second, for the platforms that emulate someone else's chip.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn virtual_machine(adapters: &[String]) -> Option<&'static str> {
    const BY_ID: [(&str, &str); 6] = [
        ("VEN_15AD", "VMware"),
        ("VEN_80EE", "VirtualBox"),
        ("VEN_1414", "Hyper-V"),
        ("VEN_1B36", "QEMU"),
        ("VEN_1AF4", "QEMU"),
        ("VEN_1AB8", "Parallels"),
    ];
    const BY_NAME: [(&str, &str); 6] = [
        ("vmware", "VMware"),
        ("virtualbox", "VirtualBox"),
        ("hyper-v", "Hyper-V"),
        ("qemu", "QEMU"),
        ("virtio", "QEMU"),
        ("parallels", "Parallels"),
    ];
    for a in adapters {
        let upper = a.to_uppercase();
        if let Some((_, who)) = BY_ID.iter().find(|(id, _)| upper.contains(id)) {
            return Some(who);
        }
        let lower = a.to_lowercase();
        if let Some((_, who)) = BY_NAME.iter().find(|(name, _)| lower.contains(name)) {
            return Some(who);
        }
    }
    None
}

/// The mode to switch to, and why in the caller's words.
///
/// `prefer` is only consulted for a virtual machine. It never goes below `min`:
/// asking for a smaller screen than the interface needs would be answering the
/// wrong question.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn choose(modes: &[Mode], min: (u32, u32), prefer: (u32, u32), virt: bool) -> Option<Mode> {
    if !virt {
        return biggest(modes);
    }
    // The target is the preferred size, unless the caller demands more.
    let (w, h) = if (min.0 as u64 * min.1 as u64) > (prefer.0 as u64 * prefer.1 as u64) {
        min
    } else {
        prefer
    };

    // Exactly what was asked for, if the driver has it.
    let exact = modes
        .iter()
        .filter(|m| m.width == w && m.height == h && m.bpp >= 32)
        .max_by_key(|m| (m.bpp, m.sane_hz(), m.hz));
    if let Some(m) = exact {
        return Some(*m);
    }
    // Otherwise the largest that still fits inside it.
    let inside = modes
        .iter()
        .filter(|m| m.usable() && m.width <= w && m.height <= h)
        .max_by_key(|m| (m.area(), m.bpp, m.sane_hz()));
    if let Some(m) = inside {
        return Some(*m);
    }
    // Nothing fits: the smallest thing above it beats the biggest.
    modes
        .iter()
        .filter(|m| m.usable())
        .min_by_key(|m| (m.area(), m.bpp))
        .copied()
        .or_else(|| biggest(modes))
}

/// The biggest true-colour mode on offer. Refresh rate breaks ties, but only
/// among sane ones: a driver that lists 1 Hz means "unspecified", and picking
/// that would be a worse screen, not a better one.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn biggest(modes: &[Mode]) -> Option<Mode> {
    modes
        .iter()
        .filter(|m| m.usable())
        .max_by_key(|m| (m.area(), m.bpp as u64, m.sane_hz() as u64, m.hz as u64))
        .copied()
        .or_else(|| {
            // No true colour at all (a very basic framebuffer): take the
            // biggest thing there is rather than refusing to help.
            modes.iter().max_by_key(|m| m.area()).copied()
        })
}

#[cfg(target_os = "windows")]
pub async fn run(args: DisplayArgs) -> own_backup_common::Result<()> {
    use own_backup_common::OwnBackupError;

    let (min, prefer) = args.sizes().ok_or_else(|| {
        OwnBackupError::Config(format!(
            "не разобрать размер {:?}/{:?} — ожидается ШИРИНАxВЫСОТА",
            args.min, args.prefer
        ))
    })?;

    let current = imp::current_mode();
    match &current {
        Some(m) => println!("Сейчас: {}x{} {} бит, {} Гц", m.width, m.height, m.bpp, m.hz),
        None => println!("Сейчас: режим определить не удалось"),
    }
    // Which adapter is actually drawing. "Microsoft Basic Display" here with a
    // vendor driver in the image means the driver did not start — the mode
    // list below belongs to the fallback, not the hardware. A virtual adapter
    // presents one head per possible monitor, so identical lines are folded.
    let adapters = imp::adapters();
    let mut shown: Vec<&String> = Vec::new();
    for a in &adapters {
        if !shown.contains(&a) {
            let same = adapters.iter().filter(|x| *x == a).count();
            println!("Адаптер: {a}{}", if same > 1 { format!(" (и ещё {})", same - 1) } else { String::new() });
            shown.push(a);
        }
    }
    let virt = virtual_machine(&adapters);

    if args.diag {
        imp::diag();
        return Ok(());
    }

    let modes = imp::available_modes();
    if args.list {
        for m in &modes {
            println!("  {}x{} {} бит, {} Гц", m.width, m.height, m.bpp, m.hz);
        }
        println!("Всего режимов: {}", modes.len());
        return Ok(());
    }

    let use_prefer = virt.is_some() && !args.biggest;
    if let Some(who) = virt {
        if use_prefer {
            println!(
                "Виртуальная машина ({who}): список режимов — каталог драйвера, а не описание \
                 экрана, поэтому берём {}x{} (изменить: --prefer ШxВ, максимум: --biggest).",
                prefer.0, prefer.1
            );
        } else {
            println!("Виртуальная машина ({who}), запрошен максимум (--biggest).");
        }
    }

    let Some(best) = choose(&modes, min, prefer, use_prefer) else {
        println!("Драйвер дисплея не перечислил ни одного режима — оставляем как есть.");
        return Ok(());
    };

    // Already there? Then there is nothing to do — and in a virtual machine
    // that is the only reason to leave the screen alone, since the chosen mode
    // is deliberately not the biggest one.
    let same_size =
        current.as_ref().is_some_and(|c| c.width == best.width && c.height == best.height);
    let good_enough = same_size
        || (!use_prefer
            && current.as_ref().is_some_and(|c| {
                c.width >= min.0 && c.height >= min.1 && c.area() >= best.area()
            }));
    if good_enough {
        println!("Менять нечего: текущий режим — тот, что нужен.");
        return Ok(());
    }
    if best.width < min.0 || best.height < min.1 {
        println!(
            "Лучшее, что умеет дисплей — {}x{}, это меньше требуемых {}x{}. Переключаемся всё равно.",
            best.width, best.height, min.0, min.1
        );
    }

    match imp::apply(&best) {
        Ok(()) => println!(
            "Переключено на {}x{} {} бит, {} Гц",
            best.width, best.height, best.bpp, best.hz
        ),
        Err(e) => {
            return Err(OwnBackupError::Device(format!(
                "не удалось переключить режим на {}x{}: {e}",
                best.width, best.height
            )))
        }
    }
    Ok(())
}

/// "1280x720" → (1280, 720). Also accepts "1280X720" and "1280*720".
///
/// Lives outside the Windows-only path so its tests run with the rest of the
/// suite, on Linux.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn parse_size(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.split_once(['x', 'X', '*'])?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

#[cfg(target_os = "windows")]
mod imp {
    use super::Mode;
    use windows::core::PCWSTR;
    use windows::Win32::Graphics::Gdi::{
        ChangeDisplaySettingsExW, EnumDisplaySettingsW, CDS_TYPE, DEVMODEW, DEVMODE_FIELD_FLAGS,
        DISP_CHANGE_SUCCESSFUL, DM_BITSPERPEL, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH,
        ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_MODE,
    };

    fn empty_devmode() -> DEVMODEW {
        DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() }
    }

    fn read(mode: ENUM_DISPLAY_SETTINGS_MODE) -> Option<Mode> {
        let mut dm = empty_devmode();
        // SAFETY: `dm` is a live DEVMODEW with dmSize filled in; a null device
        // name means "the display this session is on".
        let ok = unsafe { EnumDisplaySettingsW(PCWSTR::null(), mode, &mut dm) };
        ok.as_bool().then(|| Mode {
            width: dm.dmPelsWidth,
            height: dm.dmPelsHeight,
            bpp: dm.dmBitsPerPel,
            hz: dm.dmDisplayFrequency,
        })
    }

    pub fn current_mode() -> Option<Mode> {
        read(ENUM_CURRENT_SETTINGS)
    }

    /// Every mode the display driver admits to, deduplicated.
    pub fn available_modes() -> Vec<Mode> {
        let mut out = Vec::new();
        for i in 0.. {
            let Some(m) = read(ENUM_DISPLAY_SETTINGS_MODE(i)) else { break };
            if !out.contains(&m) {
                out.push(m);
            }
            if i > 4096 {
                break; // a driver that never says "no" is not going to start now
            }
        }
        out
    }

    /// The display adapters Windows knows of, one line each: name, device id,
    /// and whether that one is on the desktop.
    pub fn adapters() -> Vec<String> {
        use windows::Win32::Graphics::Gdi::{EnumDisplayDevicesW, DISPLAY_DEVICEW};
        let mut out = Vec::new();
        for i in 0..16u32 {
            let mut dd = DISPLAY_DEVICEW {
                cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
                ..Default::default()
            };
            // SAFETY: `dd` is a live DISPLAY_DEVICEW with cb filled in.
            if !unsafe { EnumDisplayDevicesW(PCWSTR::null(), i, &mut dd, 0) }.as_bool() {
                break;
            }
            let text = |b: &[u16]| {
                let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
                String::from_utf16_lossy(&b[..n])
            };
            let mut line = text(&dd.DeviceString);
            let id = text(&dd.DeviceID);
            if !id.is_empty() {
                line.push_str(&format!(" [{id}]"));
            }
            // DISPLAY_DEVICE_ATTACHED_TO_DESKTOP = 0x1: this one is drawing.
            if dd.StateFlags & 0x1 != 0 {
                line.push_str(" — активен");
            }
            out.push(line);
        }
        out
    }

    /// The whole story for a screen stuck at the wrong resolution: the driver
    /// packages in the image, and every line PnP wrote about the display
    /// device. Made for reading over a screenshot from a virtual machine.
    pub fn diag() {
        let windir = std::env::var("SystemRoot").unwrap_or_else(|_| r"X:\Windows".into());
        let inf_dir = std::path::Path::new(&windir).join("INF");

        println!("\nПакеты драйверов в образе ({}):", inf_dir.display());
        let mut found = false;
        if let Ok(rd) = std::fs::read_dir(&inf_dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.to_lowercase().starts_with("oem") && name.to_lowercase().ends_with(".inf") {
                    let text = read_inf(&e.path());
                    let hint = ["vm3d", "vmxnet", "pvscsi", "vmmouse"]
                        .iter()
                        .find(|k| text.to_lowercase().contains(**k))
                        .copied()
                        .unwrap_or("");
                    println!("  {name} {}", if hint.is_empty() { "" } else { hint });
                    found = true;
                }
            }
        }
        if !found {
            println!("  ни одного oem*.inf — драйверы в образ не попали");
        }

        // What PnP said. The log is the answer to "installed but not driving":
        // it records the match, the copy, and the start — or which one failed.
        let log = std::path::Path::new(&windir).join("INF").join("setupapi.dev.log");
        println!("\nsetupapi.dev.log — строки про дисплей:");
        match std::fs::read_to_string(&log) {
            Ok(text) => {
                let keys = ["vm3d", "ven_15ad", "display", "basicdisplay"];
                let picked: Vec<&str> = text
                    .lines()
                    .filter(|l| {
                        let low = l.to_lowercase();
                        keys.iter().any(|k| low.contains(k))
                    })
                    .collect();
                for l in picked.iter().rev().take(60).rev() {
                    println!("  {l}");
                }
                if picked.is_empty() {
                    println!("  про дисплей — ничего (PnP его не трогал)");
                }
            }
            Err(e) => println!("  не прочитать {}: {e}", log.display()),
        }
    }

    /// An INF as text — ANSI or UTF-16, both must read.
    fn read_inf(p: &std::path::Path) -> String {
        let Ok(bytes) = std::fs::read(p) else { return String::new() };
        if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
            let units: Vec<u16> =
                bytes[2..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            String::from_utf16_lossy(&units)
        } else {
            String::from_utf8_lossy(&bytes).to_string()
        }
    }

    pub fn apply(mode: &Mode) -> std::result::Result<(), String> {
        let mut dm = empty_devmode();
        dm.dmPelsWidth = mode.width;
        dm.dmPelsHeight = mode.height;
        dm.dmBitsPerPel = mode.bpp;
        dm.dmDisplayFrequency = mode.hz;
        dm.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_BITSPERPEL | DM_DISPLAYFREQUENCY;
        // A mode change without CDS_UPDATEREGISTRY: boot media has no registry
        // worth persisting to, and the next boot starts from the firmware's
        // choice again anyway.
        // SAFETY: `dm` is a live DEVMODEW describing an enumerated mode.
        let r = unsafe { ChangeDisplaySettingsExW(PCWSTR::null(), Some(&dm), None, CDS_TYPE(0), None) };
        if r == DISP_CHANGE_SUCCESSFUL {
            return Ok(());
        }
        // Some drivers reject an exact refresh rate but accept the resolution.
        dm.dmFields = DEVMODE_FIELD_FLAGS(DM_PELSWIDTH.0 | DM_PELSHEIGHT.0 | DM_BITSPERPEL.0);
        // SAFETY: as above.
        let r2 = unsafe { ChangeDisplaySettingsExW(PCWSTR::null(), Some(&dm), None, CDS_TYPE(0), None) };
        if r2 == DISP_CHANGE_SUCCESSFUL {
            Ok(())
        } else {
            Err(format!("ChangeDisplaySettingsEx вернул {} (и {} без частоты)", r.0, r2.0))
        }
    }
}

#[cfg(not(target_os = "windows"))]
pub async fn run(_args: DisplayArgs) -> own_backup_common::Result<()> {
    Err(own_backup_common::OwnBackupError::Config(
        "Смена режима экрана нужна только на загрузочном носителе Windows.".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(width: u32, height: u32) -> Mode {
        Mode { width, height, bpp: 32, hz: 60 }
    }

    /// The list a VMware guest really offers (2026-08-01, VMware SVGA 3D).
    fn vmware_modes() -> Vec<Mode> {
        [
            (640, 480), (800, 600), (1024, 768), (1152, 864), (1280, 960), (1400, 1050),
            (1600, 1200), (1920, 1440), (2048, 1536), (2560, 1920), (854, 480), (720, 480),
            (720, 576), (800, 480), (1280, 720), (1366, 768), (1920, 1080), (2560, 1440),
            (3840, 2160), (1280, 800), (1440, 900), (1680, 1050), (1920, 1200), (2560, 1600),
            (2880, 1800), (3840, 2400), (1280, 768), (1280, 1024),
        ]
        .into_iter()
        .map(|(w, h)| m(w, h))
        .collect()
    }

    #[test]
    fn a_size_is_read_the_way_people_write_it() {
        assert_eq!(parse_size("1280x720"), Some((1280, 720)));
        assert_eq!(parse_size("1920X1080"), Some((1920, 1080)));
        assert_eq!(parse_size(" 1024 * 768 "), Some((1024, 768)));
        assert_eq!(parse_size("1280"), None, "no separator is not a size");
        assert_eq!(parse_size("axb"), None);
    }

    /// A virtual machine is recognised by the adapter's vendor id, which is
    /// what the line from a real medium looks like.
    #[test]
    fn a_virtual_display_is_recognised() {
        let vmware = vec![
            "VMware SVGA 3D [PCI\\VEN_15AD&DEV_0405&SUBSYS_040515AD&REV_00] — активен".to_string(),
        ];
        assert_eq!(virtual_machine(&vmware), Some("VMware"));
        assert_eq!(
            virtual_machine(&["Microsoft Hyper-V Video".to_string()]),
            Some("Hyper-V")
        );
        assert_eq!(
            virtual_machine(&["Intel(R) UHD Graphics 630 [PCI\\VEN_8086&DEV_3E92]".into()]),
            None,
            "real hardware must not be taken for a guest"
        );
    }

    /// The 49-inch ultra-wide case: the guest offers 3840×2400, which says
    /// nothing about the window the guest is shown in.
    #[test]
    fn in_a_virtual_machine_the_asked_for_size_wins_over_the_biggest() {
        let chosen = choose(&vmware_modes(), (1280, 720), (1440, 900), true).unwrap();
        assert_eq!((chosen.width, chosen.height), (1440, 900));

        // The same list on real hardware: the biggest is the monitor's own.
        let chosen = choose(&vmware_modes(), (1280, 720), (1440, 900), false).unwrap();
        assert_eq!((chosen.width, chosen.height), (3840, 2400));
    }

    /// Asked for something the driver does not have: the largest that fits
    /// inside it, never something bigger.
    #[test]
    fn a_size_that_is_not_on_offer_rounds_down() {
        let chosen = choose(&vmware_modes(), (1280, 720), (1500, 950), true).unwrap();
        assert_eq!((chosen.width, chosen.height), (1440, 900));
    }

    /// `--min` above the preferred size is a demand, not a suggestion.
    #[test]
    fn the_minimum_is_never_undercut_by_the_preference() {
        let chosen = choose(&vmware_modes(), (1920, 1080), (1440, 900), true).unwrap();
        assert_eq!((chosen.width, chosen.height), (1920, 1080));
    }

    /// A driver with nothing large enough still gets an answer.
    #[test]
    fn when_nothing_fits_the_smallest_above_is_taken() {
        let modes = vec![m(1920, 1080), m(2560, 1440)];
        let chosen = choose(&modes, (800, 600), (1024, 768), true).unwrap();
        assert_eq!((chosen.width, chosen.height), (1920, 1080));
    }

    #[test]
    fn an_empty_list_chooses_nothing() {
        assert_eq!(choose(&[], (1280, 720), (1440, 900), true), None);
        assert_eq!(choose(&[], (1280, 720), (1440, 900), false), None);
    }
}
