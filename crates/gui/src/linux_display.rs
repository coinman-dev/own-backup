//! Linux: how WebKitGTK draws the page, decided before GTK starts.
//!
//! WebKitGTK 2.42+ draws through DMA-BUF buffers on the GPU's render node. That
//! fails as an empty or grey window, or as `Gdk-Message: Error 71 (Protocol
//! error)` on Wayland, where there is no usable render node (a virtual machine
//! without 3D, a container) and with NVIDIA's own driver. The stand ran the
//! window with `WEBKIT_DISABLE_DMABUF_RENDERER=1` for exactly that reason.
//! Here that switch is thrown only where it is needed:
//!
//! - whatever the person set themselves is left alone;
//! - no render node this user can open, or NVIDIA's driver loaded: the
//!   DMA-BUF renderer is off from the start;
//! - a start that never drew its page (the marker below is still there, and
//!   no window holds its name) makes the next starts of this session draw the
//!   safe way: no DMA-BUF and no compositing. One blank window, not one at
//!   every start.
//!
//! The marker is written once the window holds its name ([`starting`]) and
//! removed by `gui_ready`, which the page calls once it has drawn — the only
//! witness that the interface is alive.

use std::path::{Path, PathBuf};

const DMABUF: &str = "WEBKIT_DISABLE_DMABUF_RENDERER";
const COMPOSITING: &str = "WEBKIT_DISABLE_COMPOSITING_MODE";
/// Written at start, removed once the page has drawn.
const STARTING: &str = "gui-starting";
/// This session draws the safe way after a start that never drew.
const SAFE: &str = "gui-safe-render";

/// What was decided, for the log.
#[derive(Debug, PartialEq, Eq)]
pub enum Render {
    /// The person's own setting.
    AsSet,
    /// WebKitGTK's default (DMA-BUF where it can).
    Default,
    /// DMA-BUF off: nothing to draw it with.
    NoRenderNode,
    /// DMA-BUF off: NVIDIA's driver.
    Nvidia,
    /// DMA-BUF and compositing off: an earlier start did not draw.
    Safe,
}

/// What the machine looks like, as far as this decision goes.
struct Machine {
    user_set: bool,
    render_node: bool,
    nvidia: bool,
    previous_start_blank: bool,
}

fn decide(m: &Machine) -> Render {
    if m.user_set {
        Render::AsSet
    } else if m.previous_start_blank {
        Render::Safe
    } else if !m.render_node {
        Render::NoRenderNode
    } else if m.nvidia {
        Render::Nvidia
    } else {
        Render::Default
    }
}

/// Decide and set the environment for WebKitGTK. Called first thing in
/// `main`, while the process is still one thread, and before this window
/// takes its name: a name held by someone else means a start in progress,
/// not a blank one.
pub fn prepare() -> Render {
    let dir = crate::instance_linux::runtime_dir().ok();
    let safe = dir.as_ref().map(|d| d.join(SAFE));
    let blank = dir.as_ref().is_some_and(|d| d.join(STARTING).exists())
        && !crate::instance_linux::held("gui");
    if blank {
        if let Some(safe) = &safe {
            let _ = std::fs::write(safe, b"");
        }
    }
    let machine = Machine {
        user_set: std::env::var_os(DMABUF).is_some() || std::env::var_os(COMPOSITING).is_some(),
        render_node: render_node_usable(Path::new("/dev/dri")),
        nvidia: Path::new("/sys/module/nvidia_drm").exists(),
        previous_start_blank: safe.as_ref().is_some_and(|s| s.exists()),
    };
    let render = decide(&machine);
    // SAFETY (set_var): still single-threaded — nothing has been spawned yet.
    match render {
        Render::NoRenderNode | Render::Nvidia => std::env::set_var(DMABUF, "1"),
        Render::Safe => {
            std::env::set_var(DMABUF, "1");
            std::env::set_var(COMPOSITING, "1");
        }
        Render::AsSet | Render::Default => {}
    }
    render
}

/// This window holds its name and is about to draw: the marker that tells
/// the next start whether it did. Not written by a second copy that only
/// hands its start to the first — it would leave a marker nobody removes.
pub fn starting() {
    if let Ok(dir) = crate::instance_linux::runtime_dir() {
        let _ = std::fs::write(dir.join(STARTING), std::process::id().to_string());
    }
}

/// The page has drawn: the start was not a blank one.
pub fn drawn() {
    if let Ok(dir) = crate::instance_linux::runtime_dir() {
        let _ = std::fs::remove_file(dir.join(STARTING));
    }
}

/// Is there a render node (`/dev/dri/renderD*`) this user can open? That is
/// what the DMA-BUF renderer allocates its buffers on.
fn render_node_usable(dri: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dri) else {
        return false;
    };
    entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("renderD"))
        .map(|e| e.path())
        .any(|path: PathBuf| {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .is_ok()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine() -> Machine {
        Machine {
            user_set: false,
            render_node: true,
            nvidia: false,
            previous_start_blank: false,
        }
    }

    #[test]
    fn webkit_keeps_its_default_where_it_can_draw() {
        assert_eq!(decide(&machine()), Render::Default);
    }

    #[test]
    fn no_render_node_or_nvidia_turns_dmabuf_off() {
        let m = Machine {
            render_node: false,
            ..machine()
        };
        assert_eq!(decide(&m), Render::NoRenderNode);
        let m = Machine {
            nvidia: true,
            ..machine()
        };
        assert_eq!(decide(&m), Render::Nvidia);
    }

    #[test]
    fn a_blank_start_makes_the_next_ones_safe_and_the_person_decides_first() {
        let m = Machine {
            previous_start_blank: true,
            ..machine()
        };
        assert_eq!(decide(&m), Render::Safe);
        let m = Machine {
            user_set: true,
            previous_start_blank: true,
            render_node: false,
            ..machine()
        };
        assert_eq!(decide(&m), Render::AsSet);
    }

    #[test]
    fn a_folder_without_render_nodes_has_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("card0"), b"").unwrap();
        assert!(!render_node_usable(dir.path()));
        std::fs::write(dir.path().join("renderD128"), b"").unwrap();
        assert!(render_node_usable(dir.path()));
        assert!(!render_node_usable(&dir.path().join("missing")));
    }
}
