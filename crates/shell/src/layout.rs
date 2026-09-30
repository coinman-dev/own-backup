//! The strip's arithmetic and its captions — everything decidable without a
//! window, so it can be read and tested on any machine.
//!
//! Nothing here calls Windows. The Win32 half measures the real text and hands
//! the widths in; this module only decides where the pieces go. That split is
//! what lets a layout mistake fail in a test instead of on a medium that has to
//! be rebuilt before it can be looked at again.

/// A rectangle inside the strip, in its own client coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Slot {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Slot {
    /// Is this point inside? The right and bottom edges are outside, the way
    /// Windows treats a rectangle everywhere else.
    pub fn contains(&self, x: i32, y: i32) -> bool {
        !self.is_empty() && x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }

    pub fn right(&self) -> i32 {
        self.x + self.w
    }

    /// Nothing can be drawn in it and nothing can be clicked in it.
    pub fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }
}

/// Where the strip's four parts sit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BarSlots {
    /// The Start button's place. Until the menu exists the product name sits
    /// here, so the strip does not look broken at its left end.
    pub start: Slot,
    /// Buttons for the open windows — whatever room is left over, and the only
    /// part that gives way when there is not enough.
    pub buttons: Slot,
    /// The keyboard layout of the window in front. Clickable: it is also how
    /// the layout is switched.
    pub layout: Slot,
    /// The clock.
    pub clock: Slot,
    /// A way to close the strip — and EMPTY on the boot medium, where there
    /// must not be one: the shell exiting there means the machine reboots.
    ///
    /// Off the medium it is the opposite: this program is run on a working
    /// machine to look at, it covers the desktop and sits over the real
    /// taskbar, and a shell with no way out is a shell somebody has to reach
    /// for Task Manager to be rid of.
    pub close: Slot,
}

/// How tall the strip is on a screen this tall.
///
/// One fixed height is wrong at both ends: 40 pixels is a band across a
/// 1024x768 virtual machine and a thread on the 2560x1440 monitor this medium
/// is built for. So it is a share of the screen, bounded on both sides — below
/// the lower bound the text stops being readable, above the upper one the strip
/// starts taking space the rescue tool needs.
pub fn bar_height(screen_h: i32) -> i32 {
    let want = (screen_h as f32 * 0.033).round() as i32;
    want.clamp(32, 56)
}

/// Font cell height for a strip that tall. Text and strip grow together, or a
/// big screen gets a thick strip with tiny writing on it.
pub fn font_height(bar_h: i32) -> i32 {
    ((bar_h as f32 * 0.42).round() as i32).max(11)
}

/// Space between the strip's parts, and from its ends.
pub fn gap(bar_h: i32) -> i32 {
    (bar_h / 5).max(4)
}

/// Lay the strip out, given the widths the Win32 half measured.
///
/// `bar_w` is the width the strip actually GOT, which is not always the
/// screen's: an installed Windows may hand back a narrower rectangle to keep
/// clear of its own taskbar, and a clock placed by screen width then sits
/// outside the window entirely — found by running it on exactly such a machine.
///
/// A `close_w` of zero means no close button, which is what the boot medium
/// asks for. The order of squeezing is the rest of the point: the clock and the
/// layout keep their width whatever happens, and the window buttons take what
/// is left — possibly nothing. A clock that vanishes on a narrow screen would
/// be a bug; a window button that does not fit is a fact of life.
pub fn bar_slots(
    bar_w: i32,
    bar_h: i32,
    start_w: i32,
    layout_w: i32,
    clock_w: i32,
    close_w: i32,
) -> BarSlots {
    let g = gap(bar_h);
    let start = Slot {
        x: g,
        y: 0,
        w: start_w.max(0),
        h: bar_h,
    };

    // Right to left: the close button if there is one, then the clock, then
    // the layout.
    let mut edge = bar_w - g;
    let close = Slot {
        x: edge - close_w.max(0),
        y: 0,
        w: close_w.max(0),
        h: bar_h,
    };
    if !close.is_empty() {
        edge = close.x - g;
    }
    let clock = Slot {
        x: edge - clock_w.max(0),
        y: 0,
        w: clock_w.max(0),
        h: bar_h,
    };
    let layout = Slot {
        x: clock.x - g - layout_w.max(0),
        y: 0,
        w: layout_w.max(0),
        h: bar_h,
    };

    let from = start.right() + g;
    let buttons = Slot {
        x: from,
        y: 0,
        w: (layout.x - g - from).max(0),
        h: bar_h,
    };
    BarSlots {
        start,
        buttons,
        layout,
        clock,
        close,
    }
}

/// Keep the buttons where they are: the same windows in the same places they
/// were, new ones appended, gone ones closed up.
///
/// Windows are enumerated in Z-ORDER, top of the stack first. Taken as it
/// comes, that order moves a button the moment it is used: clicking one raises
/// its window, which puts it at the top of the stack, which jumps its button to
/// the left end of the strip — so the thing you just aimed at is no longer
/// where you aimed, and the next click lands on something else. Order has to
/// come from when a window APPEARED, not from where it is in the stack.
pub fn stable_order<K: Copy + PartialEq>(previous: &[K], current: &[K]) -> Vec<K> {
    let mut order: Vec<K> = previous
        .iter()
        .copied()
        .filter(|k| current.contains(k))
        .collect();
    let fresh: Vec<K> = current
        .iter()
        .copied()
        .filter(|k| !order.contains(k))
        .collect();
    order.extend(fresh);
    order
}

/// Buttons for the open windows, across the room the strip has left.
///
/// They share that room equally, up to a width past which a button stops being
/// easier to read and only takes space from the next one. Below a floor they
/// stop being buttons at all, so when there is not enough room the strip shows
/// as many as fit rather than a row of slivers — the rest are still one
/// Alt+Tab away, which a sliver would not have helped with either.
pub fn window_buttons(area: Slot, count: usize, bar_h: i32) -> Vec<Slot> {
    if count == 0 || area.is_empty() {
        return Vec::new();
    }
    let between = (gap(bar_h) / 2).max(2);
    let floor = bar_h;
    let cap = bar_h * 5;
    let fit = ((area.w + between) / (floor + between)).max(1) as usize;
    let shown = fit.min(count);
    let each = (((area.w + between) / shown as i32) - between).min(cap);
    (0..shown)
        .map(|n| Slot {
            x: area.x + n as i32 * (each + between),
            y: area.y,
            w: each,
            h: area.h,
        })
        .collect()
}

/// Two-letter label for the language half of a keyboard layout identifier.
///
/// The low word of an `HKL` is a language identifier, and its low ten bits are
/// the primary language. The medium installs two layouts — English and Russian
/// ([`crate::layout`]'s whole reason for existing is that nothing on screen says
/// which is active while a BitLocker passphrase is being typed) — but the image
/// can be built from any Windows, so the table covers what might plausibly turn
/// up.
///
/// Anything unknown is shown as its number rather than guessed at. Two wrong
/// letters here are worse than four honest hex digits: the indicator is only
/// worth having if it is believed.
///
/// Zero is not a language but the answer to a question that could not be asked
/// — there is no window in front, which happens for a moment at boot. Nothing
/// is shown then, for the same reason.
pub fn layout_label(langid: u16) -> String {
    if langid == 0 {
        return String::new();
    }
    let primary = langid & 0x03FF;
    let known = match primary {
        0x09 => "EN",
        0x19 => "RU",
        0x22 => "UK",
        0x23 => "BE",
        0x07 => "DE",
        0x0C => "FR",
        0x0A => "ES",
        0x10 => "IT",
        0x15 => "PL",
        0x1F => "TR",
        0x04 => "ZH",
        0x11 => "JA",
        0x12 => "KO",
        _ => return format!("{langid:04X}"),
    };
    known.to_string()
}

/// The clock, as the strip shows it.
///
/// Hours and minutes only. A ticking seconds field would repaint the strip
/// sixty times a minute for something nobody reads off a rescue environment.
pub fn clock_text(hour: u16, minute: u16) -> String {
    format!("{hour:02}:{minute:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the boot medium asks for: no close button.
    fn on_medium(bar_w: i32) -> BarSlots {
        bar_slots(bar_w, 48, 160, 40, 60, 0)
    }

    #[test]
    fn the_clock_is_at_the_right_edge_and_the_start_button_at_the_left() {
        let s = on_medium(1920);
        let g = gap(48);
        assert_eq!(s.start.x, g, "the start button hugs the left end");
        assert_eq!(s.clock.right(), 1920 - g, "the clock hugs the right end");
        assert!(
            s.layout.right() < s.clock.x,
            "the layout sits left of the clock"
        );
    }

    #[test]
    fn the_window_buttons_take_what_is_left_and_nothing_overlaps() {
        let s = on_medium(1920);
        assert!(
            s.buttons.x > s.start.right(),
            "buttons start after the start button"
        );
        assert!(
            s.buttons.right() < s.layout.x,
            "buttons end before the layout"
        );
        assert!(
            s.buttons.w > 1000,
            "a wide screen leaves plenty of room: {}",
            s.buttons.w
        );
    }

    /// The medium must not offer a way out of the shell, because there it is
    /// not a way out of a program but a reboot. Everywhere else it must.
    #[test]
    fn the_close_button_exists_only_when_it_is_asked_for() {
        let medium = on_medium(1920);
        assert!(
            medium.close.is_empty(),
            "no way out of the shell on the medium"
        );
        assert_eq!(
            medium.clock.right(),
            1920 - gap(48),
            "the clock takes the edge instead"
        );

        let desk = bar_slots(1920, 48, 160, 40, 60, 30);
        assert_eq!(
            desk.close.right(),
            1920 - gap(48),
            "the close button takes the edge"
        );
        assert!(
            desk.clock.right() < desk.close.x,
            "and pushes the clock left of it"
        );
        assert_eq!(desk.clock.w, 60, "without taking any of its width");
    }

    /// What must never happen is a strip that drops the clock or the layout
    /// because a screen is narrow. The window buttons are the part that yields.
    #[test]
    fn a_strip_too_narrow_for_everything_gives_up_the_window_buttons_first() {
        let squeezed = bar_slots(320, 32, 160, 40, 60, 0);
        assert!(
            squeezed.buttons.w < 40,
            "barely room for a button: {}",
            squeezed.buttons.w
        );

        let crushed = bar_slots(240, 32, 160, 40, 60, 0);
        assert!(
            crushed.buttons.is_empty(),
            "no room left at all for window buttons"
        );
        assert_eq!(crushed.clock.w, 60, "the clock still keeps its width");
        assert_eq!(crushed.layout.w, 40, "and so does the layout");
        assert_eq!(
            crushed.clock.right(),
            240 - gap(32),
            "and the clock its place"
        );
    }

    #[test]
    fn the_strip_is_a_share_of_the_screen_within_reason() {
        assert_eq!(bar_height(1440), 48, "the monitor this is built for");
        assert_eq!(bar_height(1080), 36);
        assert_eq!(
            bar_height(768),
            32,
            "small screens stop at the readable floor"
        );
        assert_eq!(
            bar_height(2160),
            56,
            "big ones stop before the strip takes over"
        );
        assert!(
            font_height(bar_height(768)) >= 11,
            "still readable at the floor"
        );
    }

    #[test]
    fn a_click_belongs_to_at_most_one_part() {
        let s = bar_slots(1920, 48, 160, 40, 60, 30);
        for x in [0, 5, 100, 200, 900, 1700, 1800, 1850, 1900, 1919] {
            let hits = [s.start, s.buttons, s.layout, s.clock, s.close]
                .iter()
                .filter(|slot| slot.contains(x, 10))
                .count();
            assert!(hits <= 1, "x={x} landed in {hits} parts at once");
        }
    }

    /// The bug this exists for: clicking a button raised its window, which put
    /// it first in the enumeration, which moved its button to the left end.
    #[test]
    fn using_a_button_does_not_move_it() {
        let shown = ['a', 'b', 'c'];
        // The same three windows, enumerated again after 'c' was raised.
        let raised_c = ['c', 'a', 'b'];
        assert_eq!(stable_order(&shown, &raised_c), vec!['a', 'b', 'c']);
        // And again after 'a' was raised: still nothing moves.
        assert_eq!(stable_order(&shown, &['a', 'c', 'b']), vec!['a', 'b', 'c']);
    }

    #[test]
    fn a_new_window_goes_on_the_end_and_a_closed_one_leaves_a_gap_that_closes() {
        assert_eq!(
            stable_order(&['a', 'b'], &['d', 'a', 'b']),
            vec!['a', 'b', 'd'],
            "the new one is last however the system enumerated it"
        );
        assert_eq!(stable_order(&['a', 'b', 'c'], &['c', 'a']), vec!['a', 'c']);
        assert_eq!(stable_order(&['a', 'b'], &[]), Vec::<char>::new());
    }

    #[test]
    fn the_first_look_takes_what_it_is_given() {
        assert_eq!(stable_order(&[], &['b', 'a']), vec!['b', 'a']);
    }

    #[test]
    fn window_buttons_share_the_room_without_touching_each_other() {
        let area = Slot {
            x: 200,
            y: 0,
            w: 900,
            h: 48,
        };
        let buttons = window_buttons(area, 3, 48);
        assert_eq!(buttons.len(), 3);
        assert_eq!(buttons[0].x, 200, "the first starts where the room does");
        for pair in buttons.windows(2) {
            assert!(pair[1].x > pair[0].right(), "buttons must not touch");
            assert_eq!(pair[0].w, pair[1].w, "and they are the same width");
        }
        assert!(
            buttons[2].right() <= area.right(),
            "and stay inside the room"
        );
    }

    /// Two windows on a wide screen should not give two buttons half a metre
    /// wide each.
    #[test]
    fn a_button_stops_growing_once_it_is_wide_enough_to_read() {
        let area = Slot {
            x: 200,
            y: 0,
            w: 2000,
            h: 48,
        };
        let buttons = window_buttons(area, 2, 48);
        assert_eq!(buttons[0].w, 48 * 5, "capped");
        assert!(
            buttons[1].x < area.right(),
            "and still laid out left to right"
        );
    }

    /// Slivers help nobody: better to show the ones that fit and leave the rest
    /// to Alt+Tab.
    #[test]
    fn too_many_windows_show_as_many_as_can_still_be_read() {
        let area = Slot {
            x: 0,
            y: 0,
            w: 300,
            h: 48,
        };
        let buttons = window_buttons(area, 20, 48);
        assert!(buttons.len() < 20, "not all of them");
        assert!(!buttons.is_empty(), "but not none either");
        for b in &buttons {
            assert!(b.w >= 40, "each one still wide enough to read: {}", b.w);
            assert!(b.right() <= area.right());
        }
    }

    #[test]
    fn no_windows_means_no_buttons() {
        assert!(window_buttons(
            Slot {
                x: 0,
                y: 0,
                w: 900,
                h: 48
            },
            0,
            48
        )
        .is_empty());
        assert!(
            window_buttons(Slot::default(), 3, 48).is_empty(),
            "and no room means none"
        );
    }

    #[test]
    fn the_two_layouts_the_medium_installs_have_names() {
        // 0x0409 en-US and 0x0419 ru-RU — the pair `INPUT_LOCALES` sets.
        assert_eq!(layout_label(0x0409), "EN");
        assert_eq!(layout_label(0x0419), "RU");
        // Sub-language does not change the answer: en-GB is still English.
        assert_eq!(layout_label(0x0809), "EN");
    }

    #[test]
    fn a_layout_we_do_not_know_is_shown_as_its_number_not_guessed() {
        assert_eq!(layout_label(0x0425), "0425", "Estonian is not in the table");
    }

    /// At boot there is a moment with no window in front at all, and no layout
    /// to report. An indicator showing something then would be making it up.
    #[test]
    fn nothing_in_front_means_nothing_shown() {
        assert_eq!(layout_label(0), "");
    }

    #[test]
    fn the_clock_keeps_both_digits() {
        assert_eq!(clock_text(9, 5), "09:05");
        assert_eq!(clock_text(23, 59), "23:59");
        assert_eq!(clock_text(0, 0), "00:00");
    }
}
