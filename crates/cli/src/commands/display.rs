//! Pick a screen mode worth showing an interface on.
//!
//! Boot media comes up in whatever the firmware left behind — 1024×768 on a
//! VMware guest, 800×600 on some real machines — and an interface built for a
//! desktop has nowhere to put itself. Windows itself does not choose for us:
//! WinPE has no display settings, no monitor profile and no user to ask.
//!
//! On real hardware the choice is the one a person would make. It used to be
//! read as "the largest true-colour mode the driver offers", on the theory that
//! a monitor's EDID makes that its own preferred one — see below for why that
//! did not survive a real monitor.
//!
//! **In a virtual machine that reasoning does not hold.** The guest driver's
//! list is a catalogue of what it can be *told* to do — a VMware guest offers
//! twenty-eight modes up to 3840×2400 — and not one of them describes the
//! window the guest is actually displayed in. Picking the biggest there fills
//! the host's screen with a window nobody can use; worse on a 49-inch
//! ultra-wide, where the host's own screen is not that shape either. So in a
//! virtual machine a size that fits anywhere is aimed for instead of the
//! maximum, and `--biggest` is there for whoever disagrees.
//!
//! **On real hardware "the biggest mode" was wrong too** (2026-09-17). A
//! Samsung C49G95T in picture-by-picture mode shows each input as half the
//! panel, 2560×1440, while a GPU driver still lists modes the input will not
//! take — the medium switched, and the monitor went dark. So the monitor is
//! asked first: the preferred mode it declares (its EDID, as Windows parsed
//! it), which is the one a person would pick in Settings. The largest mode is
//! only the fallback for a display that declares nothing.
//!
//! And any switch is provisional, the way Windows' own "Keep these display
//! settings?" is: the old mode comes back unless a key is pressed, because a
//! monitor that lost the signal cannot show the question.
//!
//! **Nothing may be decided while the display stack is still coming up**
//! (2026-09-17, boot medium). `drvload` returns when the INF is loaded, PnP
//! installs the device afterwards, and the mode was picked ONE SECOND into
//! that: `EnumDisplayDevices` answered with nothing at all, the monitor
//! declared nothing, and the mode list was still the basic driver's — whose
//! largest entry, 2560×1600, a 32:9 panel in picture-by-picture mode cannot
//! show. So the stack is waited for first, and when the monitor still says
//! nothing the screen is left alone unless it is too small to work in.

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

    /// Take the largest mode on offer, ignoring the monitor's own preference
    /// (and, in a virtual machine, the size aimed for).
    #[arg(long)]
    pub biggest: bool,

    /// Keep a new mode without asking. By default the old mode comes back
    /// unless a key is pressed — a dark screen cannot show the question.
    #[arg(long)]
    pub no_confirm: bool,

    /// How long to wait for that key, in seconds.
    #[arg(long, default_value_t = 15)]
    pub confirm_secs: u64,

    /// How long to wait for a display driver PnP is still installing, in
    /// seconds. Only waited out when a display package is in the image at all;
    /// 0 asks whatever is up right now.
    #[arg(long, default_value_t = 20)]
    pub settle_secs: u64,

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

/// Whether the screen is being drawn by Microsoft Basic Display — no vendor
/// driver started, so the mode is the one the boot loader set.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn is_basic_display(adapters: &[String]) -> bool {
    adapters
        .iter()
        .any(|a| a.to_uppercase().contains(r"ROOT\BASICDISPLAY") && a.ends_with("активен"))
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

/// The mode to use for a monitor that declared its preferred one.
///
/// Exactly that size when the driver has it; otherwise the largest mode that
/// fits inside it — never bigger, which is how the screen went dark. Among
/// modes of the chosen size the refresh rate nearest the monitor's own wins,
/// and a tie goes to the lower one: a monitor that asks for 60 Hz is known to
/// take 60 Hz.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn choose_preferred(modes: &[Mode], preferred: Mode) -> Option<Mode> {
    let (w, h) = (preferred.width, preferred.height);
    let exact: Vec<&Mode> =
        modes.iter().filter(|m| m.width == w && m.height == h && m.bpp >= 32).collect();
    let pool: Vec<&Mode> = if !exact.is_empty() {
        exact
    } else {
        let fits = modes.iter().filter(|m| m.usable() && m.width <= w && m.height <= h);
        let Some(area) = fits.clone().map(|m| m.area()).max() else { return None };
        fits.filter(|m| m.area() == area).collect()
    };
    let hz_gap = |m: &Mode| (m.hz as i64 - preferred.hz as i64).unsigned_abs();
    pool.into_iter()
        .min_by_key(|m| (hz_gap(m), m.hz, std::cmp::Reverse(m.bpp)))
        .copied()
}

/// The mode to take when the monitor declared nothing and the screen is too
/// small to work in.
///
/// The SMALLEST mode that still meets `min` — not the largest on offer. A
/// driver lists what it can be told to do; without the monitor's own answer
/// the only safe assumption is that an ordinary, modest size is displayable.
/// Taking the largest is what blanked a 32:9 panel with 2560×1600 on
/// 2026-09-17. `--biggest` is there for whoever wants the old behaviour.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn choose_conservative(modes: &[Mode], min: (u32, u32)) -> Option<Mode> {
    modes
        .iter()
        .filter(|m| m.usable() && m.width >= min.0 && m.height >= min.1)
        .min_by_key(|m| (m.area(), !m.sane_hz(), m.hz))
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
    // Wait for the display stack before anything is read from it. A diagnostic
    // run asks for the state as it is right now and never waits.
    let vendor_inf = imp::vendor_display_inf();
    let (adapters, waited_ms) = if args.diag || args.settle_secs == 0 {
        (imp::adapters(), 0)
    } else {
        imp::wait_for_display_stack(
            vendor_inf.as_deref(),
            std::time::Duration::from_secs(args.settle_secs),
        )
    };
    if waited_ms >= 1000 {
        println!("Ждали драйвер экрана: {:.1} с", waited_ms as f64 / 1000.0);
    }
    if adapters.is_empty() {
        println!("Windows не назвала ни одного видеоадаптера — экран ещё поднимается.");
    }
    let mut shown: Vec<&String> = Vec::new();
    for a in &adapters {
        if !shown.contains(&a) {
            let same = adapters.iter().filter(|x| *x == a).count();
            println!("Адаптер: {a}{}", if same > 1 { format!(" (и ещё {})", same - 1) } else { String::new() });
            shown.push(a);
        }
    }
    let virt = virtual_machine(&adapters);
    // Everything that decides the mode goes to the log as well as the screen:
    // on the medium the screen may be dark by the time anyone reads it, and the
    // log is copied to the boot stick.
    tracing::info!(current = ?current, adapters = ?adapters, virtual_machine = ?virt,
                   vendor_inf = ?vendor_inf, waited_ms,
                   "display: the screen as found");

    if args.diag {
        imp::diag();
        return Ok(());
    }

    let modes = imp::available_modes();
    // What the monitor itself declares. Not asked in a virtual machine, whose
    // "monitor" is a window of whatever size the host happens to give it.
    let preferred = if virt.is_none() { imp::preferred_mode() } else { None };
    tracing::info!(
        count = modes.len(),
        preferred = ?preferred,
        modes = %modes.iter().map(|m| format!("{}x{}@{}", m.width, m.height, m.hz)).collect::<Vec<_>>().join(" "),
        "display: what the driver and the monitor offer"
    );
    if args.list {
        for m in &modes {
            println!("  {}x{} {} бит, {} Гц", m.width, m.height, m.bpp, m.hz);
        }
        println!("Всего режимов: {}", modes.len());
        match preferred {
            Some(p) => println!("Монитор предпочитает: {}x{}, {} Гц", p.width, p.height, p.hz),
            None => println!("Монитор свой режим не сообщил"),
        }
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

    let from_monitor = if use_prefer || args.biggest {
        None
    } else {
        preferred.and_then(|p| choose_preferred(&modes, p))
    };
    let current_meets_min =
        current.as_ref().is_some_and(|c| c.width >= min.0 && c.height >= min.1);
    let best = match from_monitor {
        Some(m) => {
            let p = preferred.unwrap_or(m);
            println!(
                "Монитор сообщает свой режим: {}x{}, {} Гц — берём {}x{}, {} Гц.",
                p.width, p.height, p.hz, m.width, m.height, m.hz
            );
            m
        }
        // A virtual machine, or someone who asked for the maximum outright.
        None if use_prefer || args.biggest => {
            let Some(m) = choose(&modes, min, prefer, use_prefer) else {
                println!("Драйвер дисплея не перечислил ни одного режима — оставляем как есть.");
                return Ok(());
            };
            m
        }
        // Real hardware, and the monitor declared nothing. A working screen is
        // not worth risking for a bigger one nobody vouched for.
        None if current_meets_min => {
            let now = current
                .as_ref()
                .map(|c| format!("{}x{}", c.width, c.height))
                .unwrap_or_else(|| "текущий".into());
            println!(
                "Монитор свой режим не сообщил, а {now} для интерфейса хватает — оставляем как есть \
                 (принудительно: --biggest)."
            );
            tracing::info!("display: monitor silent, current mode usable — left alone");
            return Ok(());
        }
        None => {
            let Some(m) = choose_conservative(&modes, min) else {
                println!("Драйвер дисплея не перечислил ни одного режима — оставляем как есть.");
                return Ok(());
            };
            println!(
                "Монитор свой режим не сообщил: берём самый скромный подходящий — {}x{}.",
                m.width, m.height
            );
            m
        }
    };
    tracing::info!(chosen = ?best, from_monitor = from_monitor.is_some(), "display: chosen");

    // Already there? Then there is nothing to do — and in a virtual machine
    // that is the only reason to leave the screen alone, since the chosen mode
    // is deliberately not the biggest one.
    // Only the size matters here: a mode of the same size at another refresh
    // rate is not worth a switch the monitor might not survive.
    let same_size =
        current.as_ref().is_some_and(|c| c.width == best.width && c.height == best.height);
    if same_size {
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
        Ok(()) => {
            println!("Переключено на {}x{} {} бит, {} Гц", best.width, best.height, best.bpp, best.hz);
            tracing::info!(mode = ?best, "display: switched");
            if let (false, Some(prev)) = (args.no_confirm, current) {
                confirm_or_revert(prev, args.confirm_secs);
            }
        }
        // Expected, not a failure: on UEFI the basic driver lists the modes the
        // firmware knows but can only draw in the one the loader set. The medium
        // asks the loader for the highest one (`highestmode` in its BCD), so
        // there is nothing left to do here — and a red ERROR at every boot of
        // the medium read as the medium being broken.
        Err(e) if is_basic_display(&adapters) => {
            tracing::info!(error = %e, "basic display driver refused a mode change");
            println!(
                "Базовый видеодрайвер не меняет режим после загрузки — экран остаётся {}. \
                 Разрешение задаёт загрузчик носителя.",
                current
                    .as_ref()
                    .map(|c| format!("{}x{}", c.width, c.height))
                    .unwrap_or_else(|| "как есть".into())
            );
        }
        Err(e) => {
            return Err(OwnBackupError::Device(format!(
                "не удалось переключить режим на {}x{}: {e}",
                best.width, best.height
            )))
        }
    }
    Ok(())
}

/// Keep the new mode only if someone can see it.
///
/// A switch the monitor cannot display leaves a dark screen and no way to say
/// so — which is how the medium ended on 2026-09-17. So, like Windows' own
/// "Keep these display settings?", the previous mode comes back unless a key
/// is pressed in time. Without a console to read from (the command run from a
/// script with its input redirected) there is nobody to ask, and the mode is
/// kept.
#[cfg(target_os = "windows")]
fn confirm_or_revert(prev: Mode, secs: u64) {
    println!(
        "Если изображение видно, нажмите любую клавишу. Иначе через {secs} с вернётся \
         прежний режим {}x{}.",
        prev.width, prev.height
    );
    match imp::wait_for_key(std::time::Duration::from_secs(secs)) {
        Some(true) => {
            println!("Режим оставлен.");
            tracing::info!("display: new mode confirmed with a key");
        }
        Some(false) => {
            tracing::info!(back_to = ?prev, "display: no key in time — reverting");
            match imp::apply(&prev) {
                Ok(()) => println!("Клавишу не нажали — возвращён режим {}x{}.", prev.width, prev.height),
                Err(e) => {
                    tracing::info!(error = %e, "display: revert failed");
                    println!("Вернуть прежний режим не удалось: {e}");
                }
            }
        }
        None => tracing::info!("display: no console to ask — new mode kept"),
    }
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

    fn wide_str(b: &[u16]) -> String {
        let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        String::from_utf16_lossy(&b[..n])
    }

    /// GDI name of the primary display (`\\.\DISPLAY1`).
    fn primary_device_name() -> Option<String> {
        use windows::Win32::Graphics::Gdi::{
            EnumDisplayDevicesW, DISPLAY_DEVICEW, DISPLAY_DEVICE_PRIMARY_DEVICE,
        };
        for i in 0..16u32 {
            let mut dd = DISPLAY_DEVICEW {
                cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
                ..Default::default()
            };
            // SAFETY: `dd` is a live DISPLAY_DEVICEW with cb filled in.
            if !unsafe { EnumDisplayDevicesW(PCWSTR::null(), i, &mut dd, 0) }.as_bool() {
                break;
            }
            if dd.StateFlags & DISPLAY_DEVICE_PRIMARY_DEVICE != 0 {
                return Some(wide_str(&dd.DeviceName));
            }
        }
        None
    }

    /// A display driver of this machine's own, sitting in the image as an
    /// `oem*.inf`. Its presence is the reason to wait: PnP may still be
    /// installing it while this runs, and until it does the mode list belongs
    /// to Microsoft Basic Display.
    pub fn vendor_display_inf() -> Option<String> {
        let windir = std::env::var("SystemRoot").unwrap_or_else(|_| r"X:\Windows".into());
        let dir = std::path::Path::new(&windir).join("INF");
        for e in std::fs::read_dir(&dir).ok()?.flatten() {
            let name = e.file_name().to_string_lossy().to_ascii_lowercase();
            if !(name.starts_with("oem") && name.ends_with(".inf")) {
                continue;
            }
            let text = read_inf(&e.path()).to_ascii_lowercase();
            // `Class=Display` is how a graphics package announces itself. The
            // inbox fallbacks are not what anyone is waiting for.
            if text.contains("class=display")
                && !text.contains("basicdisplay")
                && !text.contains("basicrender")
            {
                return Some(name);
            }
        }
        None
    }

    /// Wait until the display stack answers, and — when a display package is in
    /// the image — until the driver from it has taken the screen over.
    ///
    /// Returns the adapters as they finally are and how long that took. On the
    /// medium of 2026-09-17 this was decided one second into the install:
    /// `EnumDisplayDevices` returned NOTHING, so neither "which adapter" nor
    /// "is this a virtual machine" could be answered, and the modes on offer
    /// were still the basic driver's.
    pub fn wait_for_display_stack(
        vendor_inf: Option<&str>,
        timeout: std::time::Duration,
    ) -> (Vec<String>, u64) {
        let start = std::time::Instant::now();
        loop {
            let found = adapters();
            let waited = start.elapsed();
            // Nothing answered yet: the device is being reinstalled.
            let ready = !found.is_empty()
                // With a package of our own in the image, the basic driver is a
                // stage and not the answer.
                && (vendor_inf.is_none() || !super::is_basic_display(&found));
            if ready || waited >= timeout {
                if !ready {
                    tracing::info!(
                        waited_ms = waited.as_millis() as u64,
                        vendor_inf = ?vendor_inf,
                        adapters = ?found,
                        "display: the stack never settled — deciding on what there is"
                    );
                }
                return (found, waited.as_millis() as u64);
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }

    /// The mode the monitor on the primary display asks for — its preferred
    /// timing as the display stack parsed it from EDID/DisplayID, which is
    /// where a 5120×1440 panel's real size lives (a base EDID block cannot
    /// even describe a width over 4095).
    pub fn preferred_mode() -> Option<Mode> {
        use windows::Win32::Devices::Display::{
            DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
            DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
            DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_PREFERRED_MODE, DISPLAYCONFIG_MODE_INFO,
            DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
            DISPLAYCONFIG_TARGET_PREFERRED_MODE, QDC_ONLY_ACTIVE_PATHS,
        };
        use windows::Win32::Foundation::ERROR_SUCCESS;

        let (mut np, mut nm) = (0u32, 0u32);
        // SAFETY: plain out-parameters.
        let sizes = unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm) };
        if sizes != ERROR_SUCCESS {
            // Logged, not swallowed: "the monitor said nothing" and "we could
            // not ask" lead to the same silence on screen and to very
            // different fixes.
            tracing::info!(code = sizes.0, "display: GetDisplayConfigBufferSizes failed");
            return None;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut infos = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        // SAFETY: the arrays are as large as the counts passed alongside them.
        let r = unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut np,
                paths.as_mut_ptr(),
                &mut nm,
                infos.as_mut_ptr(),
                None,
            )
        };
        if r != ERROR_SUCCESS {
            tracing::info!(code = r.0, paths = np, "display: QueryDisplayConfig failed");
            return None;
        }
        paths.truncate(np as usize);
        if paths.is_empty() {
            tracing::info!("display: no active display path — nothing to ask about the monitor");
            return None;
        }

        let primary = primary_device_name();
        let mut first = None;
        for p in &paths {
            let mut src = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            src.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            src.header.size = std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
            src.header.adapterId = p.sourceInfo.adapterId;
            src.header.id = p.sourceInfo.id;
            // SAFETY: the header is the first field of a correctly sized packet.
            let source = if unsafe { DisplayConfigGetDeviceInfo(&mut src.header) } == 0 {
                wide_str(&src.viewGdiDeviceName)
            } else {
                String::new()
            };

            let mut pref = DISPLAYCONFIG_TARGET_PREFERRED_MODE::default();
            pref.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_PREFERRED_MODE;
            pref.header.size = std::mem::size_of::<DISPLAYCONFIG_TARGET_PREFERRED_MODE>() as u32;
            pref.header.adapterId = p.targetInfo.adapterId;
            pref.header.id = p.targetInfo.id;
            // SAFETY: as above.
            let got = unsafe { DisplayConfigGetDeviceInfo(&mut pref.header) };
            if got != 0 || pref.width == 0 {
                tracing::info!(
                    source = %source,
                    code = got,
                    width = pref.width,
                    "display: this path declared no preferred mode"
                );
                continue;
            }
            let v = pref.targetMode.targetVideoSignalInfo.vSyncFreq;
            let hz = if v.Denominator > 0 {
                (v.Numerator as f64 / v.Denominator as f64).round() as u32
            } else {
                0
            };
            let mode = Mode { width: pref.width, height: pref.height, bpp: 32, hz };
            tracing::info!(source = %source, mode = ?mode, "display: monitor's preferred mode");
            if primary.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(&source)) {
                return Some(mode);
            }
            first.get_or_insert(mode);
        }
        first
    }

    /// Wait for a key press on the console. `None` when there is no console to
    /// read from, `Some(false)` when the time ran out.
    pub fn wait_for_key(timeout: std::time::Duration) -> Option<bool> {
        use windows::Win32::Foundation::WAIT_OBJECT_0;
        use windows::Win32::System::Console::{
            FlushConsoleInputBuffer, GetConsoleMode, GetStdHandle, ReadConsoleInputW,
            CONSOLE_MODE, INPUT_RECORD, KEY_EVENT, STD_INPUT_HANDLE,
        };
        use windows::Win32::System::Threading::WaitForSingleObject;

        // SAFETY: plain handle query.
        let h = unsafe { GetStdHandle(STD_INPUT_HANDLE) }.ok()?;
        let mut mode = CONSOLE_MODE(0);
        // SAFETY: `h` is this process's input handle; `mode` is an out-param.
        unsafe { GetConsoleMode(h, &mut mode) }.ok()?;
        // A key pressed before the question was asked is not an answer to it.
        // SAFETY: as above.
        let _ = unsafe { FlushConsoleInputBuffer(h) };

        let deadline = std::time::Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return Some(false);
            }
            // SAFETY: a valid handle and a finite timeout.
            if unsafe { WaitForSingleObject(h, left.as_millis() as u32) } != WAIT_OBJECT_0 {
                return Some(false);
            }
            let mut recs = [INPUT_RECORD::default(); 16];
            let mut n = 0u32;
            // SAFETY: the buffer and its length travel together.
            if unsafe { ReadConsoleInputW(h, &mut recs, &mut n) }.is_err() {
                return Some(false);
            }
            for r in &recs[..n as usize] {
                // SAFETY: the union is read as the member EventType says it is.
                if r.EventType as u32 == KEY_EVENT && unsafe { r.Event.KeyEvent.bKeyDown }.as_bool() {
                    return Some(true);
                }
            }
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

    /// The line a real medium printed on Windows 11's WinRE (2026-09-17). Only
    /// the ACTIVE basic adapter counts: an inactive one beside a started vendor
    /// driver means that driver is drawing.
    #[test]
    fn the_basic_display_driver_is_recognised_only_when_it_draws() {
        let active = "Microsoft Basic Display Driver [ROOT\\BasicDisplay] — активен".to_string();
        assert!(is_basic_display(&[active]));
        let idle = "Microsoft Basic Display Driver [ROOT\\BasicDisplay]".to_string();
        let vendor = "NVIDIA GeForce RTX 3080 [PCI\\VEN_10DE&DEV_2206] — активен".to_string();
        assert!(!is_basic_display(&[idle, vendor]));
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

    fn mh(width: u32, height: u32, hz: u32) -> Mode {
        Mode { width, height, bpp: 32, hz }
    }

    /// The case that blacked the screen: a 32:9 panel in picture-by-picture
    /// mode declares 2560×1440 per input, the driver lists far more. The
    /// monitor's own answer wins, at its own refresh rate.
    #[test]
    fn the_monitor_preferred_mode_beats_the_biggest() {
        let modes = vec![
            mh(1920, 1080, 60), mh(2560, 1440, 60), mh(2560, 1440, 120),
            mh(3840, 2160, 60), mh(5120, 1440, 60), mh(5120, 1440, 240),
        ];
        let chosen = choose_preferred(&modes, mh(2560, 1440, 60)).unwrap();
        assert_eq!(chosen, mh(2560, 1440, 60));
        // The same monitor as one big screen.
        let chosen = choose_preferred(&modes, mh(5120, 1440, 60)).unwrap();
        assert_eq!(chosen, mh(5120, 1440, 60));
    }

    /// A preferred size the driver does not list: the largest that fits inside
    /// it, never anything bigger.
    #[test]
    fn a_preferred_size_not_on_offer_rounds_down_not_up() {
        let modes = vec![mh(1920, 1080, 60), mh(2560, 1080, 60), mh(3840, 2160, 60)];
        let chosen = choose_preferred(&modes, mh(2560, 1440, 60)).unwrap();
        assert_eq!((chosen.width, chosen.height), (2560, 1080));
        assert_eq!(choose_preferred(&[mh(3840, 2160, 60)], mh(2560, 1440, 60)), None);
    }

    /// A refresh rate that is not listed exactly: the nearest, and on a tie the
    /// lower one.
    #[test]
    fn the_refresh_rate_nearest_the_monitors_wins() {
        let modes = vec![mh(2560, 1440, 59), mh(2560, 1440, 75), mh(2560, 1440, 144)];
        assert_eq!(choose_preferred(&modes, mh(2560, 1440, 60)).unwrap().hz, 59);
        let modes = vec![mh(2560, 1440, 50), mh(2560, 1440, 70)];
        assert_eq!(choose_preferred(&modes, mh(2560, 1440, 60)).unwrap().hz, 50);
    }

    /// The list the basic display driver really offered on the boot medium
    /// (2026-09-17). The monitor is a 32:9 panel showing 2560×1440 per input;
    /// nothing in this list describes it, and the largest entry blanked it.
    fn winpe_basic_modes() -> Vec<Mode> {
        [
            (1024, 768), (640, 480), (800, 600), (1152, 864), (1280, 768), (1280, 800),
            (1280, 1024), (1440, 900), (1600, 1200), (1680, 1050), (1856, 1392),
            (1920, 1200), (2560, 1600), (1600, 768), (1600, 900), (1920, 1080),
            (1024, 600), (1366, 768),
        ]
        .into_iter()
        .map(|(w, h)| mh(w, h, 60))
        .collect()
    }

    #[test]
    fn with_no_word_from_the_monitor_the_modest_mode_wins() {
        let chosen = choose_conservative(&winpe_basic_modes(), (1280, 720)).unwrap();
        assert_eq!((chosen.width, chosen.height), (1280, 768));
        // The trap this exists for.
        assert_eq!(biggest(&winpe_basic_modes()).unwrap().width, 2560);
    }

    #[test]
    fn nothing_meets_the_minimum_so_the_biggest_is_better_than_nothing() {
        let modes = vec![mh(800, 600, 60), mh(1024, 768, 60)];
        assert_eq!(choose_conservative(&modes, (1280, 720)).unwrap().width, 1024);
        assert_eq!(choose_conservative(&[], (1280, 720)), None);
    }

    #[test]
    fn an_empty_list_chooses_nothing() {
        assert_eq!(choose(&[], (1280, 720), (1440, 900), true), None);
        assert_eq!(choose(&[], (1280, 720), (1440, 900), false), None);
    }
}
