//! What is in the Start menu — decided before anything is drawn, so the
//! decision can be tested rather than clicked.
//!
//! The menu is short on purpose. It is the whole of what a rescue environment
//! needs to offer besides the product itself: a console, the product, the tools
//! the build happened to find, and the two ways of ending the session.

use own_backup_common::tr;

/// One line of the menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub id: u32,
    pub text: String,
    /// A separator is drawn above this item.
    pub separator_above: bool,
}

pub const COMMAND_PROMPT: u32 = 1;
pub const INTERFACE: u32 = 2;
/// The Linux medium's terminal (`foot`), its counterpart of the console.
#[cfg_attr(windows, allow(dead_code))]
pub const TERMINAL: u32 = 3;
pub const REBOOT: u32 = 90;
pub const SHUTDOWN: u32 = 91;
pub const CLOSE: u32 = 92;
/// Tool *n* of [`TOOLS`] is `TOOL_BASE + n`.
pub const TOOL_BASE: u32 = 10;

/// Programs the build may have copied onto the medium from the machine that
/// made it, in the order they appear in the menu.
///
/// The table itself lives in `common`, because the other half of the agreement
/// — the builder that puts them there — cannot see this crate and this crate
/// cannot see it. A path written out twice is a path that eventually differs.
/// Where they are not on the medium, their lines simply never appear, which is
/// also what happens when the machine that built it had neither installed.
pub use own_backup_common::media_tools::MEDIA_TOOLS as TOOLS;

/// The menu, given where we are running and which of [`TOOLS`] were found.
///
/// `on_medium` decides the last group, and it is not a cosmetic difference.
/// On the medium the session ends by restarting or switching off the machine,
/// and there is no way out of the shell because leaving it IS the restart. On
/// an installed Windows the opposite holds: this program is run to look at, it
/// covers the desktop, and offering to reboot a working computer from a strip
/// at the bottom of the screen is how someone loses an afternoon to a stray
/// click.
pub fn items(on_medium: bool, tools_found: &[bool]) -> Vec<Item> {
    let mut out = vec![
        Item {
            id: COMMAND_PROMPT,
            text: tr!("menu.command_prompt").into_owned(),
            separator_above: false,
        },
        Item {
            id: INTERFACE,
            text: tr!("menu.interface").into_owned(),
            separator_above: false,
        },
    ];

    let mut first_tool = true;
    for (n, tool) in TOOLS.iter().enumerate() {
        if !tools_found.get(n).copied().unwrap_or(false) {
            continue;
        }
        out.push(Item {
            id: TOOL_BASE + n as u32,
            text: tool.name.into(),
            separator_above: std::mem::take(&mut first_tool),
        });
    }

    if on_medium {
        out.push(Item {
            id: REBOOT,
            text: tr!("menu.reboot").into_owned(),
            separator_above: true,
        });
        out.push(Item {
            id: SHUTDOWN,
            text: tr!("menu.shutdown").into_owned(),
            separator_above: false,
        });
    } else {
        out.push(Item {
            id: CLOSE,
            text: tr!("menu.close").into_owned(),
            separator_above: true,
        });
    }
    out
}

/// The Linux medium's programs besides the product (Krusader, FeatherPad):
/// Debian packages the build installs when asked; tool *n* is
/// `TOOL_BASE + n` here as in [`TOOLS`].
#[cfg_attr(windows, allow(unused_imports))]
pub use own_backup_common::media_tools::LINUX_MEDIA_TOOLS as LINUX_TOOLS;

/// The Linux medium's menu: the product, a terminal, the tools the build put
/// there, and the two ways of ending the session — the same rule about them
/// as in [`items`].
///
/// The product comes first here because a terminal is the rarer need on a
/// medium whose window does the work; on WinPE the console is first because
/// for years it was the medium.
#[cfg_attr(windows, allow(dead_code))]
pub fn items_linux(on_medium: bool, tools_found: &[bool]) -> Vec<Item> {
    let mut out = vec![
        Item {
            id: INTERFACE,
            text: tr!("menu.interface").into_owned(),
            separator_above: false,
        },
        Item {
            id: TERMINAL,
            text: tr!("menu.terminal").into_owned(),
            separator_above: false,
        },
    ];
    let mut first_tool = true;
    for (n, tool) in LINUX_TOOLS.iter().enumerate() {
        if !tools_found.get(n).copied().unwrap_or(false) {
            continue;
        }
        out.push(Item {
            id: TOOL_BASE + n as u32,
            text: tool.name.into(),
            separator_above: std::mem::take(&mut first_tool),
        });
    }
    if on_medium {
        out.push(Item {
            id: REBOOT,
            text: tr!("menu.reboot").into_owned(),
            separator_above: true,
        });
        out.push(Item {
            id: SHUTDOWN,
            text: tr!("menu.shutdown").into_owned(),
            separator_above: false,
        });
    } else {
        out.push(Item {
            id: CLOSE,
            text: tr!("menu.close").into_owned(),
            separator_above: true,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_linux_menu_has_the_product_a_terminal_and_the_way_out() {
        assert_eq!(
            ids(&items_linux(true, &[])),
            vec![INTERFACE, TERMINAL, REBOOT, SHUTDOWN]
        );
        assert_eq!(
            ids(&items_linux(false, &[false, false])),
            vec![INTERFACE, TERMINAL, CLOSE]
        );
        let separators: Vec<u32> = items_linux(true, &[])
            .iter()
            .filter(|i| i.separator_above)
            .map(|i| i.id)
            .collect();
        assert_eq!(separators, vec![REBOOT], "one line above the way out");
    }

    /// Krusader and FeatherPad are there only when the build was asked for
    /// them; each under its own number, the group set off by one line.
    #[test]
    fn the_linux_menu_shows_the_tools_the_medium_has() {
        let both = items_linux(true, &[true, true]);
        assert_eq!(
            ids(&both),
            vec![
                INTERFACE,
                TERMINAL,
                TOOL_BASE,
                TOOL_BASE + 1,
                REBOOT,
                SHUTDOWN
            ]
        );
        assert_eq!(both[2].text, "Krusader");
        assert_eq!(both[3].text, "FeatherPad");
        let separators: Vec<u32> = both
            .iter()
            .filter(|i| i.separator_above)
            .map(|i| i.id)
            .collect();
        assert_eq!(separators, vec![TOOL_BASE, REBOOT]);
        assert_eq!(
            ids(&items_linux(true, &[false, true])),
            vec![INTERFACE, TERMINAL, TOOL_BASE + 1, REBOOT, SHUTDOWN]
        );
    }

    fn ids(items: &[Item]) -> Vec<u32> {
        items.iter().map(|i| i.id).collect()
    }

    /// The one difference that is about safety rather than taste: a working
    /// machine must not be offered a reboot by a strip at the bottom of its
    /// screen.
    #[test]
    fn only_the_medium_offers_to_restart_the_machine() {
        let medium = items(true, &[false, false]);
        assert!(
            ids(&medium).contains(&REBOOT),
            "the medium ends by restarting"
        );
        assert!(ids(&medium).contains(&SHUTDOWN));
        assert!(
            !ids(&medium).contains(&CLOSE),
            "leaving the shell here IS the restart"
        );

        let desk = items(false, &[false, false]);
        assert!(
            !ids(&desk).contains(&REBOOT),
            "a working machine is never offered this"
        );
        assert!(!ids(&desk).contains(&SHUTDOWN));
        assert!(
            ids(&desk).contains(&CLOSE),
            "it is offered a way out instead"
        );
    }

    #[test]
    fn the_two_things_always_there_are_the_console_and_the_product() {
        for on_medium in [true, false] {
            let list = items(on_medium, &[]);
            assert_eq!(list[0].id, COMMAND_PROMPT);
            assert_eq!(list[1].id, INTERFACE);
        }
    }

    #[test]
    fn a_tool_the_build_did_not_find_is_not_in_the_menu() {
        let none = items(true, &[false, false]);
        assert!(!ids(&none).iter().any(|id| *id >= TOOL_BASE && *id < REBOOT));

        let second_only = items(true, &[false, true]);
        assert_eq!(
            ids(&second_only),
            vec![COMMAND_PROMPT, INTERFACE, TOOL_BASE + 1, REBOOT, SHUTDOWN],
            "the one that was found, under its own number"
        );
    }

    /// The tools arrive as a group, so they are told apart from the product
    /// above them by one line, not by two or none.
    #[test]
    fn the_tools_are_separated_from_the_rest_once() {
        let both = items(true, &[true, true]);
        let separators: Vec<u32> = both
            .iter()
            .filter(|i| i.separator_above)
            .map(|i| i.id)
            .collect();
        assert_eq!(
            separators,
            vec![TOOL_BASE, REBOOT],
            "one above the tools, one below"
        );
    }

    #[test]
    fn every_line_has_a_number_of_its_own() {
        let list = items(true, &[true, true]);
        let mut seen = ids(&list);
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), list.len(), "two lines share an id");
        assert!(!seen.contains(&0), "zero is what a cancelled menu answers");
    }
}
