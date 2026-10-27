//! The Linux boot medium's desktop: the strip along the bottom and the backdrop
//! behind everything, on labwc (plan §6, stage 6, "taskbar").
//!
//! The same strip as on WinPE ([`crate::layout`] places it, [`crate::menu`]
//! decides the Start menu, the colours are the WinPE ones), drawn with cairo
//! on two layer-shell surfaces: the strip on the top layer with an exclusive
//! zone, so a maximised window stops above it, and the backdrop on the
//! background layer, so a click on the empty desktop lands on us and not on
//! the compositor's own menu. The open windows come from
//! [`toplevels::Tracker`], the keyboard layout from [`keyboard::Mirror`].
//!
//! As on WinPE it launches things and draws, nothing else. It is not the
//! session: `kiosk-desktop` runs labwc, labwc's autostart runs this program in
//! a loop with a pause, and a crash here costs the strip for two seconds and
//! nothing on the screen behind it.

mod keyboard;
mod layer;
mod toplevels;

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gdk, glib, pango};
use own_backup_common::tr;

use self::layer::{Edge, Layer, LayerShell};
use crate::{layout, menu};

type Rgb = (f64, f64, f64);

const fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0)
}

// The WinPE strip's colours (win.rs): one product, one look.
const BACKDROP: Rgb = rgb(15, 48, 87);
const BACKDROP_TEXT: Rgb = rgb(110, 143, 176);
const BAR_BG: Rgb = rgb(242, 242, 242);
const BAR_EDGE: Rgb = rgb(196, 196, 196);
const BAR_TEXT: Rgb = rgb(26, 26, 26);
const BAR_BUTTON: Rgb = rgb(222, 222, 222);
const BAR_BUTTON_FRONT: Rgb = rgb(198, 208, 220);

const CLOCK_SAMPLE: &str = "00:00";
/// The widest the layout indicator gets ("EN", "RU", "UK"...).
const LAYOUT_SAMPLE: &str = "WW";
const CLOSE_TEXT: &str = "\u{2715}";
const FONT_FAMILY: &str = "Sans";

/// The window the Start menu's first line opens. The window keeps to one
/// instance itself: a second start shows the first.
const GUI: &str = "/usr/bin/own-backup-gui";
/// The medium's terminal (`package-lists`).
const TERMINAL: &str = "foot";

/// One window button, as drawn.
#[derive(Clone)]
struct Button {
    key: u32,
    caption: String,
    front: bool,
}

#[derive(Default)]
struct Bar {
    on_medium: bool,
    bar_h: i32,
    width: i32,
    language: &'static str,
    slots: layout::BarSlots,
    window_slots: Vec<layout::Slot>,
    buttons: Vec<Button>,
    clock: String,
    tracker: Option<toplevels::Tracker>,
    /// The layout indicator's text; empty while there is no indicator.
    layout: String,
    /// What was last written for the self-test and a restarted strip: the
    /// label and where the indicator is on the screen.
    saved: (String, (i32, i32)),
}

type Keyboard = Rc<RefCell<keyboard::Mirror>>;

pub fn log(message: &str) {
    // stderr is labwc's, and labwc's is the kiosk unit's journal — which the
    // medium already copies onto the stick (own-backup-media-journal).
    eprintln!("own-backup-shell: {message}");
}

pub fn run() -> Result<(), String> {
    gtk::init().map_err(|e| format!("GTK: {e}"))?;
    if !layer::is_supported() {
        return Err("the compositor offers no layer-shell".into());
    }
    let on_medium = own_backup_common::media_mode::is_linux_medium();
    let screen_h = screen_height().ok_or("no monitor")?;
    let bar_h = layout::bar_height(screen_h);
    log(&format!(
        "starting: screen height {screen_h}, strip {bar_h}, on the medium: {on_medium}"
    ));

    let state = Rc::new(RefCell::new(Bar {
        on_medium,
        bar_h,
        ..Default::default()
    }));

    // The layout only on the medium: there the session is root and labwc's
    // keymap is the medium's own. A strip run to look at on someone's desktop
    // could neither read the keyboards nor know that desktop's layouts.
    let keyboard: Option<Keyboard> = if on_medium {
        match keyboard::Mirror::start() {
            Ok(m) => Some(Rc::new(RefCell::new(m))),
            Err(e) => {
                log(&format!("no layout indicator: {e}"));
                None
            }
        }
    } else {
        None
    };
    if let Some(kb) = &keyboard {
        state.borrow_mut().layout = layout_label(&kb.borrow());
    }

    let back = backdrop();
    let (strip, area) = strip(&state, keyboard.clone());
    if let Some(kb) = &keyboard {
        watch_keyboards(kb, &state, &area);
    }

    match toplevels::Tracker::connect() {
        Ok(tracker) => {
            let fd = tracker.fd();
            state.borrow_mut().tracker = Some(tracker);
            let (st, ar) = (state.clone(), area.clone());
            glib::source::unix_fd_add_local(
                fd,
                glib::IOCondition::IN | glib::IOCondition::ERR | glib::IOCondition::HUP,
                move |_, _| {
                    let mut bar = st.borrow_mut();
                    let Some(tracker) = bar.tracker.as_mut() else {
                        return glib::ControlFlow::Break;
                    };
                    if let Err(e) = tracker.pump() {
                        log(&format!("window list lost: {e}"));
                        bar.tracker = None;
                        bar.buttons.clear();
                        bar.window_slots.clear();
                        ar.queue_draw();
                        return glib::ControlFlow::Break;
                    }
                    if refresh_windows(&mut bar) {
                        ar.queue_draw();
                    }
                    glib::ControlFlow::Continue
                },
            );
            refresh_windows(&mut state.borrow_mut());
        }
        Err(e) => log(&format!("no window buttons: {e}")),
    }

    {
        let (st, ar, sw) = (state.clone(), area.clone(), strip.clone());
        let kb = keyboard.clone();
        tick(&st, &ar, &sw);
        glib::timeout_add_seconds_local(1, move || {
            tick(&st, &ar, &sw);
            if let Some(kb) = &kb {
                // A keyboard plugged in, a console switched back to.
                watch_keyboards(kb, &st, &ar);
                kb.borrow_mut().ours_in_front();
                show_layout(kb, &st, &ar);
            }
            glib::ControlFlow::Continue
        });
    }

    back.show_all();
    strip.show_all();
    gtk::main();
    log("closed");
    Ok(())
}

fn screen_height() -> Option<i32> {
    let display = gdk::Display::default()?;
    let monitor = display.primary_monitor().or_else(|| display.monitor(0))?;
    Some(monitor.geometry().height())
}

fn backdrop() -> gtk::Window {
    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.init_layer_shell();
    window.set_namespace("own-backup-backdrop");
    window.set_layer(Layer::Background);
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        window.set_anchor(edge, true);
    }
    // Under the strip too, not beside it.
    window.set_exclusive_zone(-1);
    let area = gtk::DrawingArea::new();
    area.connect_draw(|area, cr| {
        let (w, h) = (area.allocated_width(), area.allocated_height());
        fill(cr, 0, 0, w, h, BACKDROP);
        let size = layout::font_height(layout::bar_height(h)) * 2;
        let band = layout::Slot {
            x: 0,
            y: h / 2 - h / 20,
            w,
            h: h / 10,
        };
        text(
            area,
            cr,
            &band,
            "Own Backup",
            size,
            Align::Center,
            BACKDROP_TEXT,
        );
        glib::Propagation::Stop
    });
    window.add(&area);
    window
}

/// The indicator's text for the layout the mirror is in.
fn layout_label(kb: &keyboard::Mirror) -> String {
    layout::xkb_label_at(&kb.codes, kb.xkb.layout())
}

/// The mirror's layout onto the strip, when it changed.
fn show_layout(kb: &Keyboard, state: &Rc<RefCell<Bar>>, area: &gtk::DrawingArea) {
    let kb = kb.borrow();
    let label = layout_label(&kb);
    let mut bar = state.borrow_mut();
    if bar.layout != label {
        log(&format!("keyboard: layout {label}"));
        bar.layout = label;
        area.queue_draw();
    }
    // Where the indicator is, in screen pixels: the strip is along the
    // bottom, its height the bar's.
    let s = bar.slots.layout;
    let at = screen_height()
        .map(|h| (s.x + s.w / 2, h - bar.bar_h + s.y + s.h / 2))
        .unwrap_or_default();
    let now = (bar.layout.clone(), at);
    if bar.saved != now {
        kb.save(&now.0, now.1);
        bar.saved = now;
    }
}

/// Read the keyboards not read yet, each as its keys arrive.
fn watch_keyboards(kb: &Keyboard, state: &Rc<RefCell<Bar>>, area: &gtk::DrawingArea) {
    let added = kb.borrow_mut().scan();
    for (path, fd) in added {
        let (kb, st, ar) = (kb.clone(), state.clone(), area.clone());
        glib::source::unix_fd_add_local(
            fd,
            glib::IOCondition::IN | glib::IOCondition::ERR | glib::IOCondition::HUP,
            move |_, _| {
                if kb.borrow_mut().read(&path).is_err() {
                    return glib::ControlFlow::Break;
                }
                show_layout(&kb, &st, &ar);
                glib::ControlFlow::Continue
            },
        );
    }
}

fn strip(state: &Rc<RefCell<Bar>>, keyboard: Option<Keyboard>) -> (gtk::Window, gtk::DrawingArea) {
    let bar_h = state.borrow().bar_h;
    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.init_layer_shell();
    window.set_namespace("own-backup-shell");
    window.set_layer(Layer::Top);
    for edge in [Edge::Bottom, Edge::Left, Edge::Right] {
        window.set_anchor(edge, true);
    }
    window.set_size_request(-1, bar_h);
    window.auto_exclusive_zone_enable();

    let area = gtk::DrawingArea::new();
    area.set_size_request(-1, bar_h);
    area.add_events(gdk::EventMask::BUTTON_PRESS_MASK | gdk::EventMask::BUTTON_RELEASE_MASK);
    {
        let st = state.clone();
        area.connect_draw(move |area, cr| {
            let width = area.allocated_width();
            if width != st.borrow().width {
                reflow(&mut st.borrow_mut(), area, width);
            }
            paint(&st.borrow(), area, cr);
            glib::Propagation::Stop
        });
    }
    {
        let st = state.clone();
        // The Start menu opens on the press, and is handed that press: a
        // Wayland popup takes its grab with the serial of a button press, and
        // one opened on the release could be refused (the first click on the
        // stand, 24.10, opened nothing).
        area.connect_button_press_event(move |area, event| {
            let (x, y) = event.position();
            let start = st.borrow().slots.start;
            if event.button() == 1 && start.contains(x as i32, y as i32) {
                show_menu(st.borrow().on_medium, area, &start, event);
            }
            glib::Propagation::Stop
        });
    }
    {
        let st = state.clone();
        area.connect_button_release_event(move |area, event| {
            if event.button() == 1 {
                let (x, y) = event.position();
                let on_layout = st.borrow().slots.layout.contains(x as i32, y as i32);
                match &keyboard {
                    // As on WinPE: a click on the indicator is the next layout.
                    Some(kb) if on_layout => {
                        kb.borrow_mut().switch_layout();
                        show_layout(kb, &st, area);
                    }
                    _ => click(&st, x as i32, y as i32),
                }
            }
            glib::Propagation::Stop
        });
    }
    // The indicator and the clock (the only hint when there was no
    // indicator, plan "taskbar" 4) say how the layout is switched.
    area.set_has_tooltip(true);
    {
        let st = state.clone();
        area.connect_query_tooltip(move |_, x, y, _, tooltip| {
            let s = st.borrow().slots;
            if s.clock.contains(x, y) || s.layout.contains(x, y) {
                tooltip.set_text(Some(&tr!("linux.layout_hint")));
                true
            } else {
                false
            }
        });
    }
    window.add(&area);
    (window, area)
}

/// Once a second: the clock, the language, the screen's height.
fn tick(state: &Rc<RefCell<Bar>>, area: &gtk::DrawingArea, window: &gtk::Window) {
    let now = glib::DateTime::now_local().ok();
    let clock = now
        .map(|t| layout::clock_text(t.hour() as u16, t.minute() as u16))
        .unwrap_or_default();
    let mut bar = state.borrow_mut();
    let mut redraw = bar.clock != clock;
    bar.clock = clock;
    if let Some(h) = screen_height() {
        let bar_h = layout::bar_height(h);
        if bar_h != bar.bar_h {
            bar.bar_h = bar_h;
            window.set_size_request(-1, bar_h);
            area.set_size_request(-1, bar_h);
            bar.width = 0;
            redraw = true;
        }
    }
    if bar.language != own_backup_common::lang::code() {
        // The Start button has a new word and a new width.
        bar.width = 0;
        redraw = true;
    }
    if redraw {
        area.queue_draw();
    }
}

fn reflow(bar: &mut Bar, area: &gtk::DrawingArea, width: i32) {
    let font = layout::font_height(bar.bar_h);
    let g = layout::gap(bar.bar_h);
    let close_w = if bar.on_medium {
        0
    } else {
        measure(area, CLOSE_TEXT, font) + g
    };
    let layout_w = if bar.layout.is_empty() {
        0
    } else {
        measure(area, LAYOUT_SAMPLE, font) + g * 2
    };
    bar.slots = layout::bar_slots(
        width,
        bar.bar_h,
        measure(area, &tr!("win.start"), font) + g * 3,
        layout_w,
        measure(area, CLOCK_SAMPLE, font) + g,
        close_w,
    );
    bar.width = width;
    bar.language = own_backup_common::lang::code();
    bar.window_slots = layout::window_buttons(bar.slots.buttons, bar.buttons.len(), bar.bar_h);
}

/// Take what the compositor said into the buttons. True when they changed.
fn refresh_windows(bar: &mut Bar) -> bool {
    let Some(tracker) = bar.tracker.as_mut() else {
        return false;
    };
    if !std::mem::take(&mut tracker.state.changed) {
        return false;
    }
    let seen: Vec<&toplevels::Toplevel> =
        tracker.state.windows.iter().filter(|w| w.shown()).collect();
    let previous: Vec<u32> = bar.buttons.iter().map(|b| b.key).collect();
    let current: Vec<u32> = seen.iter().map(|w| w.key()).collect();
    let buttons: Vec<Button> = layout::stable_order(&previous, &current)
        .into_iter()
        .filter_map(|k| seen.iter().find(|w| w.key() == k))
        .map(|w| Button {
            key: w.key(),
            caption: w.caption().to_string(),
            front: w.activated && !w.minimized,
        })
        .collect();
    write_window_list(&tracker.state.windows);
    bar.buttons = buttons;
    bar.window_slots = layout::window_buttons(bar.slots.buttons, bar.buttons.len(), bar.bar_h);
    true
}

/// What the strip shows, for the medium's self-test: one line per window,
/// `front|back`, `folded|open`, the caption. In the runtime directory, so it
/// goes with the session.
fn write_window_list(windows: &[toplevels::Toplevel]) {
    let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") else {
        return;
    };
    let lines: String = windows
        .iter()
        .filter(|w| w.shown())
        .map(|w| {
            format!(
                "{}\t{}\t{}\n",
                if w.activated { "front" } else { "back" },
                if w.minimized { "folded" } else { "open" },
                w.caption().replace(['\n', '\t'], " ")
            )
        })
        .collect();
    let path = std::path::Path::new(&dir).join("own-backup-shell.windows");
    let tmp = path.with_extension("windows.tmp");
    if std::fs::write(&tmp, lines)
        .and_then(|_| std::fs::rename(&tmp, &path))
        .is_err()
    {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// A release on the strip: everything but the Start button, which opens on
/// the press.
fn click(state: &Rc<RefCell<Bar>>, x: i32, y: i32) {
    let (slots, hit) = {
        let bar = state.borrow();
        let hit = bar
            .window_slots
            .iter()
            .zip(&bar.buttons)
            .find(|(slot, _)| slot.contains(x, y))
            .map(|(_, b)| b.key);
        (bar.slots, hit)
    };
    if slots.close.contains(x, y) {
        log("closed from the strip");
        gtk::main_quit();
    } else if let Some(key) = hit {
        let mut bar = state.borrow_mut();
        if let Some(tracker) = bar.tracker.as_mut() {
            let handle = tracker
                .state
                .windows
                .iter()
                .find(|w| w.key() == key)
                .map(|w| w.handle.clone());
            if let Some(handle) = handle {
                tracker.raise_or_hide(&handle);
            }
        }
    }
}

fn show_menu(
    on_medium: bool,
    area: &gtk::DrawingArea,
    at: &layout::Slot,
    press: &gdk::EventButton,
) {
    let Some(window) = area.window() else {
        return;
    };
    let popup = gtk::Menu::new();
    // Looked at on every opening: cheap, and never stale.
    let found: Vec<bool> = menu::LINUX_TOOLS
        .iter()
        .map(|t| std::path::Path::new(t.exe).is_file())
        .collect();
    for item in menu::items_linux(on_medium, &found) {
        if item.separator_above {
            popup.append(&gtk::SeparatorMenuItem::new());
        }
        let line = gtk::MenuItem::with_label(&item.text);
        let id = item.id;
        // After the menu has gone: a confirmation opened from inside the
        // menu's own grab would fight it for the pointer.
        line.connect_activate(move |_| {
            glib::idle_add_local_once(move || chose(id));
        });
        popup.append(&line);
    }
    popup.set_attach_widget(Some(area));
    popup.show_all();
    let rect = gdk::Rectangle::new(at.x, at.y, at.w, at.h);
    popup.popup_at_rect(
        &window,
        &rect,
        gdk::Gravity::NorthWest,
        gdk::Gravity::SouthWest,
        Some(press),
    );
}

fn chose(id: u32) {
    match id {
        menu::INTERFACE => launch(GUI, &[]),
        menu::TERMINAL => launch(TERMINAL, &[]),
        menu::REBOOT => {
            if confirm(&tr!("win.confirm_reboot"), &tr!("menu.reboot")) {
                launch("systemctl", &["reboot"]);
            }
        }
        menu::SHUTDOWN => {
            if confirm(&tr!("win.confirm_shutdown"), &tr!("menu.shutdown")) {
                launch("systemctl", &["poweroff"]);
            }
        }
        menu::CLOSE => {
            log("closed from the Start menu");
            gtk::main_quit();
        }
        tool if tool >= menu::TOOL_BASE => {
            match menu::LINUX_TOOLS.get((tool - menu::TOOL_BASE) as usize) {
                Some(t) => launch_tool(t.exe),
                None => log(&format!("menu line {tool} means nothing here")),
            }
        }
        other => log(&format!("menu line {other} means nothing here")),
    }
}

/// Start a program and forget it. A thread waits for it, so it does not
/// linger as a zombie of a strip that never waits for anything itself.
fn launch(program: &str, args: &[&str]) {
    match std::process::Command::new(program).args(args).spawn() {
        Ok(mut child) => {
            log(&format!("started {program} as {}", child.id()));
            let _ = std::thread::Builder::new()
                .name("reaper".into())
                .spawn(move || {
                    let _ = child.wait();
                });
        }
        Err(error) => log(&format!("could not start {program}: {error}")),
    }
}

/// Krusader and FeatherPad are KDE and Qt programs, and those expect a
/// session bus: Krusader's file operations and both programs' "one instance"
/// go through it. The medium's session has none (no dbus-user-session), so
/// each gets a bus of its own for as long as it runs — `dbus-run-session`,
/// part of the medium's dbus. Where there is a bus, it is used as it is.
fn launch_tool(exe: &str) {
    let has_bus = std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some()
        || std::env::var_os("XDG_RUNTIME_DIR")
            .is_some_and(|d| std::path::Path::new(&d).join("bus").exists());
    if has_bus || !std::path::Path::new(DBUS_RUN_SESSION).is_file() {
        launch(exe, &[]);
    } else {
        launch(DBUS_RUN_SESSION, &["--", exe]);
    }
}

const DBUS_RUN_SESSION: &str = "/usr/bin/dbus-run-session";

/// A yes-or-no question with our own words on the buttons: the medium carries
/// no GTK translations, so stock ones would be English whatever was chosen.
fn confirm(question: &str, action: &str) -> bool {
    let dialog = gtk::MessageDialog::new(
        None::<&gtk::Window>,
        gtk::DialogFlags::MODAL,
        gtk::MessageType::Question,
        gtk::ButtonsType::None,
        question,
    );
    dialog.add_button(&tr!("linux.cancel"), gtk::ResponseType::Cancel);
    dialog.add_button(action, gtk::ResponseType::Accept);
    dialog.set_default_response(gtk::ResponseType::Cancel);
    let answer = dialog.run();
    dialog.close();
    answer == gtk::ResponseType::Accept
}

// --- drawing ------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Align {
    Left,
    Center,
    Right,
}

fn fill(cr: &gtk::cairo::Context, x: i32, y: i32, w: i32, h: i32, colour: Rgb) {
    cr.set_source_rgb(colour.0, colour.1, colour.2);
    cr.rectangle(x as f64, y as f64, w as f64, h as f64);
    let _ = cr.fill();
}

fn font(size: i32) -> pango::FontDescription {
    let mut f = pango::FontDescription::from_string(FONT_FAMILY);
    // `font_height` is the cell of a line, as Windows asks for a font; an em
    // is about four fifths of it.
    f.set_absolute_size(f64::from(size) * 0.8 * f64::from(pango::SCALE));
    f
}

fn measure(area: &gtk::DrawingArea, s: &str, size: i32) -> i32 {
    let l = area.create_pango_layout(Some(s));
    l.set_font_description(Some(&font(size)));
    l.pixel_size().0
}

fn text(
    area: &gtk::DrawingArea,
    cr: &gtk::cairo::Context,
    slot: &layout::Slot,
    s: &str,
    size: i32,
    align: Align,
    colour: Rgb,
) {
    if slot.is_empty() || s.is_empty() {
        return;
    }
    let l = area.create_pango_layout(Some(s));
    l.set_font_description(Some(&font(size)));
    l.set_width(slot.w * pango::SCALE);
    l.set_ellipsize(pango::EllipsizeMode::End);
    l.set_single_paragraph_mode(true);
    l.set_alignment(match align {
        Align::Left => pango::Alignment::Left,
        Align::Center => pango::Alignment::Center,
        Align::Right => pango::Alignment::Right,
    });
    let (_, h) = l.pixel_size();
    cr.set_source_rgb(colour.0, colour.1, colour.2);
    cr.move_to(slot.x as f64, (slot.y + (slot.h - h) / 2) as f64);
    pangocairo::functions::show_layout(cr, &l);
}

fn paint(bar: &Bar, area: &gtk::DrawingArea, cr: &gtk::cairo::Context) {
    let (w, h) = (bar.width.max(area.allocated_width()), bar.bar_h);
    fill(cr, 0, 0, w, h, BAR_BG);
    // A hairline along the top, so the strip reads as a strip and not as the
    // bottom of whatever window is above it.
    fill(cr, 0, 0, w, 1, BAR_EDGE);
    let s = &bar.slots;
    if !s.start.is_empty() {
        fill(cr, s.start.x, 2, s.start.w, h - 2, BAR_BUTTON);
    }
    for (slot, button) in bar.window_slots.iter().zip(&bar.buttons) {
        let colour = if button.front {
            BAR_BUTTON_FRONT
        } else {
            BAR_BUTTON
        };
        fill(cr, slot.x, 2, slot.w, h - 2, colour);
    }
    let size = layout::font_height(h);
    text(
        area,
        cr,
        &s.start,
        &tr!("win.start"),
        size,
        Align::Center,
        BAR_TEXT,
    );
    for (slot, button) in bar.window_slots.iter().zip(&bar.buttons) {
        // Inset, so a long title runs out of room before the next button.
        let inside = layout::Slot {
            x: slot.x + 6,
            w: (slot.w - 12).max(0),
            ..*slot
        };
        text(
            area,
            cr,
            &inside,
            &button.caption,
            size,
            Align::Left,
            BAR_TEXT,
        );
    }
    text(
        area,
        cr,
        &s.layout,
        &bar.layout,
        size,
        Align::Center,
        BAR_TEXT,
    );
    text(area, cr, &s.clock, &bar.clock, size, Align::Right, BAR_TEXT);
    text(
        area,
        cr,
        &s.close,
        CLOSE_TEXT,
        size,
        Align::Center,
        BAR_TEXT,
    );
}
