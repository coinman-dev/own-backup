//! Linux: the icon in the notification area — «the service is running», and
//! a menu that opens the window.
//!
//! The same exe started as `own-backup-gui --tray`, as on Windows: no Tauri,
//! no WebView, only GTK's main loop and the icon, which the `tray-icon` crate
//! hands to libayatana-appindicator. That speaks StatusNotifierItem, which
//! KDE Plasma shows as it is and GNOME shows with the AppIndicator extension
//! (on by default in Ubuntu). An AppIndicator has no click of its own — a
//! click opens the menu — so «Открыть Own Backup» is its first item.
//!
//! The icon is shown exactly while the service is running: its presence is
//! the answer to «is my backup service up». A person in the `own-backup`
//! group is answered by the service's socket; anyone else by systemd.
//!
//! Started at every login by the package's `/etc/xdg/autostart` entry. The
//! settings switch is one person's choice, so it is kept the way the
//! autostart specification keeps one: a file of the same name in their own
//! `~/.config/autostart` with `Hidden=true` turns the system entry off for
//! them alone.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::instance_linux::{self as instance, Claim};
use own_backup_common::config::AppConfig;

/// The autostart entry's file name: the package installs it in
/// `/etc/xdg/autostart`, and a person's override has the same name.
const AUTOSTART_FILE: &str = "own-backup-tray.desktop";
/// How often the service is asked.
const POLL: Duration = Duration::from_secs(5);
/// After a click started the window, how long another click starts nothing:
/// the window takes a moment before it holds its name and answers.
const START_GRACE: Duration = Duration::from_secs(5);
static WINDOW_STARTED: Mutex<Option<Instant>> = Mutex::new(None);

// ---- the wish (called from the window) -------------------------------------

/// `$XDG_CONFIG_HOME/autostart/own-backup-tray.desktop` (`~/.config/…`).
fn override_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("autostart").join(AUTOSTART_FILE))
}

/// Does an autostart file switch the entry off? `Hidden=true` is the
/// specification's word for it; GNOME's own key says the same.
fn turns_off(desktop_file: &str) -> bool {
    desktop_file.lines().map(str::trim).any(|line| {
        line.eq_ignore_ascii_case("Hidden=true")
            || line.eq_ignore_ascii_case("X-GNOME-Autostart-enabled=false")
    })
}

/// Whether this person wants the icon. Wanted until they say otherwise.
pub fn enabled() -> bool {
    override_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .is_none_or(|text| !turns_off(&text))
}

/// Remember the wish and act on it at once.
pub fn set_enabled(on: bool) -> Result<(), String> {
    let path = override_path().ok_or_else(|| "HOME is not set".to_string())?;
    if on {
        // Only a file that switches the entry off is ours to remove: one the
        // person wrote themselves (another Exec, a delay) stays.
        if std::fs::read_to_string(&path).is_ok_and(|text| turns_off(&text)) {
            std::fs::remove_file(&path).map_err(|e| e.to_string())?;
        }
        start_icon();
    } else {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(
            &path,
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=Own Backup (notification area)\n\
             Exec=own-backup-gui --tray\n\
             Hidden=true\n\
             X-GNOME-Autostart-enabled=false\n",
        )
        .map_err(|e| e.to_string())?;
        instance::tell("tray", "quit");
    }
    Ok(())
}

/// At every start of the window: the icon is there if it is wanted, for a
/// session that began before the package was installed or after the icon was
/// hidden from its menu. A second one finds the first and leaves.
pub fn apply() {
    if enabled() && !instance::held("tray") {
        start_icon();
    }
}

fn start_icon() {
    match std::env::current_exe() {
        Ok(exe) => {
            if let Err(error) = std::process::Command::new(&exe).arg("--tray").spawn() {
                tracing::warn!(%error, "tray: could not start the icon");
            }
        }
        Err(error) => tracing::warn!(%error, "tray: current_exe() failed"),
    }
}

// ---- the icon's own process (`--tray`) -------------------------------------

/// `own-backup-gui --tray`. Returns when the icon is asked to go.
pub fn run() {
    if own_backup_common::media_mode::is_media_mode() {
        return;
    }
    let claimed = match instance::claim("tray") {
        Ok(Claim::Taken) => return,
        Ok(Claim::Ours(claimed)) => Some(claimed),
        // Cannot tell whether another one runs: one icon too many is better
        // than none.
        Err(_) => None,
    };
    init_log();
    own_backup_common::lang::follow_config_or_system(AppConfig::default_path());
    tracing::info!(pid = std::process::id(), "tray: started");
    if let Err(error) = gtk::init() {
        tracing::error!(%error, "tray: no display, so no icon");
        return;
    }
    if let Some(claimed) = claimed {
        claimed.listen(|message| {
            if message == "quit" {
                glib::MainContext::default().invoke(gtk::main_quit);
            }
        });
    }
    Tray::start();
    gtk::main();
    tracing::info!("tray: stopped");
}

/// Its own log beside the window's, as on Windows.
fn init_log() {
    let data_dir = AppConfig::default().data_dir;
    let Some(dir) = own_backup_common::log_files::log_dir(&data_dir) else {
        return;
    };
    let log = own_backup_common::log_files::DailyLog::new(dir, "own-backup-tray");
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(Mutex::new(log))
        .try_init();
}

struct Tray {
    menu: Menu,
    open: MenuItem,
    hide: MenuItem,
    icon: Option<Icon>,
    shown: Option<TrayIcon>,
    /// The language the menu was last written in.
    language: &'static str,
    /// One line in the log for an icon that would not be made, not one per tick.
    build_failed: bool,
}

impl Tray {
    fn start() {
        let open = MenuItem::new(own_backup_common::tr!("tray.open"), true, None);
        let hide = MenuItem::new(own_backup_common::tr!("tray.hide"), true, None);
        let menu = Menu::new();
        let separator = PredefinedMenuItem::separator();
        if let Err(error) = menu.append_items(&[&open, &separator, &hide]) {
            tracing::warn!(%error, "tray: menu not built");
        }
        let (open_id, hide_id) = (open.id().clone(), hide.id().clone());
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            if event.id == open_id {
                open_window();
            } else if event.id == hide_id {
                // Until the next login or the next start of the window;
                // switching the icon off for good is in the settings.
                glib::MainContext::default().invoke(gtk::main_quit);
            }
        }));
        let tray = Rc::new(RefCell::new(Tray {
            menu,
            open,
            hide,
            icon: load_icon(),
            shown: None,
            language: own_backup_common::lang::code(),
            build_failed: false,
        }));
        tray.borrow_mut().follow_service();
        glib::timeout_add_local(POLL, move || {
            tray.borrow_mut().follow_service();
            glib::ControlFlow::Continue
        });
    }

    /// Show the icon while the service runs, take it away when it does not.
    fn follow_service(&mut self) {
        let running = service_running();
        if running && self.shown.is_none() {
            let tip = own_backup_common::tr!("tray.tooltip").into_owned();
            let mut builder = TrayIconBuilder::new()
                .with_menu(Box::new(self.menu.clone()))
                .with_tooltip(&tip)
                .with_title("Own Backup");
            if let Some(icon) = &self.icon {
                builder = builder.with_icon(icon.clone());
            }
            match builder.build() {
                Ok(shown) => {
                    self.shown = Some(shown);
                    self.build_failed = false;
                    tracing::info!("tray: the service is running, icon shown");
                }
                Err(error) => {
                    if !std::mem::replace(&mut self.build_failed, true) {
                        tracing::warn!(%error, "tray: the icon was not made, retrying");
                    }
                }
            }
        } else if !running && self.shown.is_some() {
            self.shown = None;
            tracing::info!("tray: the service is not running, icon hidden");
        }
        self.follow_language();
    }

    /// The menu follows a change of language made in the window.
    fn follow_language(&mut self) {
        let now = own_backup_common::lang::code();
        if now == self.language {
            return;
        }
        self.language = now;
        self.open.set_text(own_backup_common::tr!("tray.open"));
        self.hide.set_text(own_backup_common::tr!("tray.hide"));
        if let Some(shown) = &self.shown {
            let _ = shown.set_tooltip(Some(own_backup_common::tr!("tray.tooltip")));
        }
    }
}

/// The program's icon, from the same file the window's is made of.
fn load_icon() -> Option<Icon> {
    let decoder = png::Decoder::new(std::io::Cursor::new(include_bytes!("../icons/32x32.png")));
    let decoded = (|| {
        let mut decoder = decoder;
        decoder.set_transformations(png::Transformations::normalize_to_color8());
        let mut reader = decoder.read_info().ok()?;
        let mut buffer = vec![0; reader.output_buffer_size()];
        let frame = reader.next_frame(&mut buffer).ok()?;
        let pixels = &buffer[..frame.buffer_size()];
        let rgba: Vec<u8> = match frame.color_type {
            png::ColorType::Rgba => pixels.to_vec(),
            png::ColorType::Rgb => pixels
                .as_chunks::<3>()
                .0
                .iter()
                .flat_map(|p| [p[0], p[1], p[2], 255])
                .collect(),
            _ => return None,
        };
        Icon::from_rgba(rgba, frame.width, frame.height).ok()
    })();
    if decoded.is_none() {
        tracing::warn!("tray: the program's icon did not load, using the theme's");
    }
    decoded
}

/// Is the service up? Its socket answers a member of the access group; the
/// socket's folder is closed to anyone else, and systemd answers them.
fn service_running() -> bool {
    match std::os::unix::net::UnixStream::connect(own_backup_common::unix_socket_path()) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            std::process::Command::new("systemctl")
                .args(["is-active", "--quiet", own_backup_common::LINUX_UNIT_NAME])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        }
        Err(_) => false,
    }
}

/// Bring the window forward, or start it.
fn open_window() {
    if instance::tell("gui", "show") {
        return;
    }
    // Started but not answering yet: a second click must not start a second
    // copy, which would only hand the click to the first.
    if instance::held("gui") {
        return;
    }
    let mut started = WINDOW_STARTED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if started.is_some_and(|at| at.elapsed() < START_GRACE) {
        return;
    }
    match std::env::current_exe() {
        Ok(exe) => match std::process::Command::new(&exe).spawn() {
            Ok(_) => {
                *started = Some(Instant::now());
                tracing::info!(exe = %exe.display(), "tray: window started");
            }
            Err(error) => {
                tracing::error!(%error, exe = %exe.display(), "tray: window did not start")
            }
        },
        Err(error) => tracing::error!(%error, "tray: current_exe() failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_override_switches_the_entry_off_only_when_it_says_so() {
        assert!(turns_off("[Desktop Entry]\nHidden=true\n"));
        assert!(turns_off(
            "[Desktop Entry]\nX-GNOME-Autostart-enabled=false\n"
        ));
        assert!(!turns_off("[Desktop Entry]\nExec=own-backup-gui --tray\n"));
        assert!(!turns_off("[Desktop Entry]\nHidden=false\n"));
    }

    #[test]
    fn switching_off_writes_the_override_the_desktop_reads() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", dir.path());
        assert!(enabled());
        set_enabled(false).unwrap();
        assert!(!enabled());
        let text = std::fs::read_to_string(dir.path().join("autostart").join(AUTOSTART_FILE));
        assert!(text.unwrap().contains("Hidden=true"));
    }

    #[test]
    fn the_icon_file_decodes() {
        assert!(load_icon().is_some());
    }
}
