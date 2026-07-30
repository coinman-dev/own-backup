//! Pick a screen mode worth showing an interface on.
//!
//! Boot media comes up in whatever the firmware left behind — 1024×768 on a
//! VMware guest, 800×600 on some real machines — and an interface built for a
//! desktop has nowhere to put itself. Windows itself does not choose for us:
//! WinPE has no display settings, no monitor profile and no user to ask.
//!
//! So the mode is chosen the way a person would: ask the display driver what it
//! can do, take the largest true-colour mode it offers (which on a monitor that
//! reports its EDID is the monitor's own preferred one), and switch to it —
//! unless what is already on screen is at least as good.

use clap::Args;

#[derive(Args)]
pub struct DisplayArgs {
    /// Smallest acceptable mode, e.g. 1280x720. Nothing is changed when the
    /// screen already meets it and no better mode exists.
    #[arg(long, default_value = "1280x720")]
    pub min: String,

    /// Show what the display can do and change nothing.
    #[arg(long)]
    pub list: bool,
}

#[cfg(target_os = "windows")]
pub async fn run(args: DisplayArgs) -> own_backup_common::Result<()> {
    use own_backup_common::OwnBackupError;

    let (min_w, min_h) = parse_size(&args.min).ok_or_else(|| {
        OwnBackupError::Config(format!("не разобрать размер {:?} — ожидается ШИРИНАxВЫСОТА", args.min))
    })?;

    let current = imp::current_mode();
    match &current {
        Some(m) => println!("Сейчас: {}x{} {} бит, {} Гц", m.width, m.height, m.bpp, m.hz),
        None => println!("Сейчас: режим определить не удалось"),
    }

    let modes = imp::available_modes();
    if args.list {
        for m in &modes {
            println!("  {}x{} {} бит, {} Гц", m.width, m.height, m.bpp, m.hz);
        }
        println!("Всего режимов: {}", modes.len());
        return Ok(());
    }

    let Some(best) = imp::best_mode(&modes) else {
        println!("Драйвер дисплея не перечислил ни одного режима — оставляем как есть.");
        return Ok(());
    };
    let area = |m: &imp::DisplayMode| m.width as u64 * m.height as u64;
    let good_enough = current
        .as_ref()
        .is_some_and(|c| c.width >= min_w && c.height >= min_h && area(c) >= area(&best));
    if good_enough {
        println!("Менять нечего: текущий режим не хуже лучшего доступного.");
        return Ok(());
    }
    if best.width < min_w || best.height < min_h {
        println!(
            "Лучшее, что умеет дисплей — {}x{}, это меньше требуемых {}x{}. Переключаемся всё равно.",
            best.width, best.height, min_w, min_h
        );
    }

    match imp::apply(&best) {
        Ok(()) => println!("Переключено на {}x{} {} бит, {} Гц", best.width, best.height, best.bpp, best.hz),
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
    use windows::core::PCWSTR;
    use windows::Win32::Graphics::Gdi::{
        ChangeDisplaySettingsExW, EnumDisplaySettingsW, CDS_TYPE, DEVMODEW, DEVMODE_FIELD_FLAGS,
        DISP_CHANGE_SUCCESSFUL, DM_BITSPERPEL, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH,
        ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_MODE,
    };

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct DisplayMode {
        pub width: u32,
        pub height: u32,
        pub bpp: u32,
        pub hz: u32,
    }

    fn empty_devmode() -> DEVMODEW {
        DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() }
    }

    fn read(mode: ENUM_DISPLAY_SETTINGS_MODE) -> Option<DisplayMode> {
        let mut dm = empty_devmode();
        // SAFETY: `dm` is a live DEVMODEW with dmSize filled in; a null device
        // name means "the display this session is on".
        let ok = unsafe { EnumDisplaySettingsW(PCWSTR::null(), mode, &mut dm) };
        ok.as_bool().then(|| DisplayMode {
            width: dm.dmPelsWidth,
            height: dm.dmPelsHeight,
            bpp: dm.dmBitsPerPel,
            hz: dm.dmDisplayFrequency,
        })
    }

    pub fn current_mode() -> Option<DisplayMode> {
        read(ENUM_CURRENT_SETTINGS)
    }

    /// Every mode the display driver admits to, deduplicated.
    pub fn available_modes() -> Vec<DisplayMode> {
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

    /// The biggest true-colour mode on offer. Refresh rate breaks ties, but
    /// only among sane ones: a driver that lists 1 Hz means "unspecified", and
    /// picking that would be a worse screen, not a better one.
    pub fn best_mode(modes: &[DisplayMode]) -> Option<DisplayMode> {
        modes
            .iter()
            .filter(|m| m.bpp >= 32 && m.width >= 640 && m.height >= 480)
            .max_by_key(|m| {
                (m.width as u64 * m.height as u64, m.bpp as u64, (m.hz <= 75) as u64, m.hz as u64)
            })
            .copied()
            .or_else(|| {
                // No true colour at all (a very basic framebuffer): take the
                // biggest thing there is rather than refusing to help.
                modes.iter().max_by_key(|m| m.width as u64 * m.height as u64).copied()
            })
    }

    pub fn apply(mode: &DisplayMode) -> std::result::Result<(), String> {
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
    use super::parse_size;

    #[test]
    fn a_size_is_read_the_way_people_write_it() {
        assert_eq!(parse_size("1280x720"), Some((1280, 720)));
        assert_eq!(parse_size("1920X1080"), Some((1920, 1080)));
        assert_eq!(parse_size(" 1024 * 768 "), Some((1024, 768)));
        assert_eq!(parse_size("1280"), None, "no separator is not a size");
        assert_eq!(parse_size("axb"), None);
    }
}
