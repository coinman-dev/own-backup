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
    /// The clock, hard against the right edge.
    pub clock: Slot,
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
/// The order of squeezing is the point: the clock and the layout keep their
/// width whatever happens, and the window buttons take what is left — possibly
/// nothing. A clock that disappears on a narrow screen would be a bug; a window
/// button that does not fit is a fact of life.
pub fn bar_slots(bar_w: i32, bar_h: i32, start_w: i32, layout_w: i32, clock_w: i32) -> BarSlots {
    let g = gap(bar_h);
    let start = Slot { x: g, y: 0, w: start_w.max(0), h: bar_h };
    let clock = Slot { x: bar_w - g - clock_w.max(0), y: 0, w: clock_w.max(0), h: bar_h };
    let layout = Slot { x: clock.x - g - layout_w.max(0), y: 0, w: layout_w.max(0), h: bar_h };
    let from = start.right() + g;
    let buttons = Slot { x: from, y: 0, w: (layout.x - g - from).max(0), h: bar_h };
    BarSlots { start, buttons, layout, clock }
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

    #[test]
    fn the_clock_is_at_the_right_edge_and_the_start_button_at_the_left() {
        let s = bar_slots(1920, 48, 160, 40, 60);
        let g = gap(48);
        assert_eq!(s.start.x, g, "the start button hugs the left end");
        assert_eq!(s.clock.right(), 1920 - g, "the clock hugs the right end");
        assert!(s.layout.right() < s.clock.x, "the layout sits left of the clock");
    }

    #[test]
    fn the_window_buttons_take_what_is_left_and_nothing_overlaps() {
        let s = bar_slots(1920, 48, 160, 40, 60);
        assert!(s.buttons.x > s.start.right(), "buttons start after the start button");
        assert!(s.buttons.right() < s.layout.x, "buttons end before the layout");
        assert!(s.buttons.w > 1000, "a wide screen leaves plenty of room: {}", s.buttons.w);
    }

    /// What must never happen is a strip that drops the clock or the layout
    /// because a screen is narrow. The window buttons are the part that yields.
    #[test]
    fn a_strip_too_narrow_for_everything_gives_up_the_window_buttons_first() {
        let squeezed = bar_slots(320, 32, 160, 40, 60);
        assert!(squeezed.buttons.w < 40, "barely room for a button: {}", squeezed.buttons.w);

        let crushed = bar_slots(240, 32, 160, 40, 60);
        assert!(crushed.buttons.is_empty(), "no room left at all for window buttons");
        assert_eq!(crushed.clock.w, 60, "the clock still keeps its width");
        assert_eq!(crushed.layout.w, 40, "and so does the layout");
        assert_eq!(crushed.clock.right(), 240 - gap(32), "and the clock its place");
    }

    #[test]
    fn the_strip_is_a_share_of_the_screen_within_reason() {
        assert_eq!(bar_height(1440), 48, "the monitor this is built for");
        assert_eq!(bar_height(1080), 36);
        assert_eq!(bar_height(768), 32, "small screens stop at the readable floor");
        assert_eq!(bar_height(2160), 56, "big ones stop before the strip takes over");
        assert!(font_height(bar_height(768)) >= 11, "still readable at the floor");
    }

    #[test]
    fn a_click_belongs_to_at_most_one_part() {
        let s = bar_slots(1920, 48, 160, 40, 60);
        for x in [0, 5, 100, 200, 900, 1700, 1800, 1850, 1900, 1919] {
            let hits = [s.start, s.buttons, s.layout, s.clock]
                .iter()
                .filter(|slot| slot.contains(x, 10))
                .count();
            assert!(hits <= 1, "x={x} landed in {hits} parts at once");
        }
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
