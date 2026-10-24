//! The open windows, as the compositor tells them: wlroots' foreign-toplevel
//! protocol (`zwlr_foreign_toplevel_manager_v1`, labwc has it) on a Wayland
//! connection of our own, read from GTK's main loop.
//!
//! GTK's connection is GTK's: it knows nothing of this protocol and gives no
//! way to bind a global on it. A second connection to the same compositor is
//! what every panel of this kind does, and it costs one socket.
//!
//! Nothing here is held but what the compositor said last. The handles are the
//! compositor's; a click only ASKS it to activate or minimise a window, and the
//! strip draws what comes back, not what it asked for.

use std::os::fd::{AsFd, AsRawFd};

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{event_created_child, Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{self as handle, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self as manager, ZwlrForeignToplevelManagerV1},
};

/// The strip's own app id: GTK's default, the program's name.
const OWN_APP_ID: &str = "own-backup-shell";

/// One window, as last described by the compositor.
#[derive(Debug, Clone)]
pub struct Toplevel {
    pub handle: ZwlrForeignToplevelHandleV1,
    pub title: String,
    pub app_id: String,
    pub activated: bool,
    pub minimized: bool,
    /// The compositor finished its first description (`done`): a window that
    /// has not had one yet has no title to show.
    described: bool,
}

/// What the dispatching fills in. `changed` is the strip's cue to redraw.
#[derive(Default)]
pub struct State {
    pub windows: Vec<Toplevel>,
    pub changed: bool,
    pub finished: bool,
}

/// The connection, its queue, and the two globals a click needs.
pub struct Tracker {
    conn: Connection,
    queue: EventQueue<State>,
    pub state: State,
    seat: Option<wl_seat::WlSeat>,
    _manager: ZwlrForeignToplevelManagerV1,
}

impl Tracker {
    /// Connect and ask for the window list. `Err` when there is no compositor
    /// or it does not offer the protocol — the strip then has no window
    /// buttons, and everything else on it still works.
    pub fn connect() -> Result<Self, String> {
        let conn = Connection::connect_to_env().map_err(|e| format!("no Wayland: {e}"))?;
        let (globals, mut queue) =
            registry_queue_init::<State>(&conn).map_err(|e| format!("registry: {e}"))?;
        let qh = queue.handle();
        let manager: ZwlrForeignToplevelManagerV1 = globals
            .bind(&qh, 1..=3, ())
            .map_err(|e| format!("no zwlr_foreign_toplevel_manager_v1: {e}"))?;
        let seat: Option<wl_seat::WlSeat> = globals.bind(&qh, 1..=7, ()).ok();
        let mut state = State::default();
        queue
            .roundtrip(&mut state)
            .map_err(|e| format!("first roundtrip: {e}"))?;
        Ok(Self {
            conn,
            queue,
            state,
            seat,
            _manager: manager,
        })
    }

    /// The socket to wait on.
    pub fn fd(&self) -> std::os::fd::RawFd {
        self.conn.as_fd().as_raw_fd()
    }

    /// Read what arrived and dispatch it. `Err` when the connection is gone.
    pub fn pump(&mut self) -> Result<(), String> {
        if let Some(guard) = self.queue.prepare_read() {
            // WouldBlock is the normal answer when GLib woke us for nothing.
            if let Err(e) = guard.read() {
                if !matches!(&e, wayland_client::backend::WaylandError::Io(io)
                    if io.kind() == std::io::ErrorKind::WouldBlock)
                {
                    return Err(format!("read: {e}"));
                }
            }
        }
        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(|e| format!("dispatch: {e}"))?;
        self.flush()
    }

    fn flush(&self) -> Result<(), String> {
        self.conn.flush().map_err(|e| format!("flush: {e}"))
    }

    /// The window buttons' rule, as on the WinPE strip: the window in front
    /// folds away, any other comes forward (unfolded first if it was folded).
    pub fn raise_or_hide(&mut self, which: &ZwlrForeignToplevelHandleV1) {
        let Some(w) = self.state.windows.iter().find(|w| &w.handle == which) else {
            return;
        };
        if w.activated && !w.minimized {
            w.handle.set_minimized();
        } else {
            if w.minimized {
                w.handle.unset_minimized();
            }
            match &self.seat {
                Some(seat) => w.handle.activate(seat),
                None => crate::linux::log("no seat: a window cannot be brought forward"),
            }
        }
        if let Err(e) = self.flush() {
            crate::linux::log(&e);
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Globals that come and go later (a second seat, an output) do not
        // change what the strip needs.
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrForeignToplevelManagerV1,
        event: manager::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            manager::Event::Toplevel { toplevel } => state.windows.push(Toplevel {
                handle: toplevel,
                title: String::new(),
                app_id: String::new(),
                activated: false,
                minimized: false,
                described: false,
            }),
            manager::Event::Finished => {
                state.finished = true;
                state.changed = true;
            }
            _ => {}
        }
    }

    event_created_child!(State, ZwlrForeignToplevelManagerV1, [
        manager::EVT_TOPLEVEL_OPCODE => (ZwlrForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ZwlrForeignToplevelHandleV1,
        event: handle::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let handle::Event::Closed = event {
            state.windows.retain(|w| &w.handle != proxy);
            proxy.destroy();
            state.changed = true;
            return;
        }
        let Some(w) = state.windows.iter_mut().find(|w| &w.handle == proxy) else {
            return;
        };
        match event {
            handle::Event::Title { title } => w.title = title,
            handle::Event::AppId { app_id } => w.app_id = app_id,
            handle::Event::State { state: raw } => {
                let (activated, minimized) = states(&raw);
                w.activated = activated;
                w.minimized = minimized;
            }
            handle::Event::Done => {
                w.described = true;
                state.changed = true;
            }
            _ => {}
        }
    }
}

impl Toplevel {
    /// Shown on the strip: described at least once, and with something to
    /// call it by.
    ///
    /// Not the strip's own windows (its confirmation dialog): a button that
    /// opens the question already on the screen is noise.
    pub fn shown(&self) -> bool {
        self.described
            && !(self.title.is_empty() && self.app_id.is_empty())
            && self.app_id != OWN_APP_ID
    }

    /// The button's caption: the title, or the program's id when the window
    /// has none.
    pub fn caption(&self) -> &str {
        if self.title.is_empty() {
            &self.app_id
        } else {
            &self.title
        }
    }

    /// A key for [`crate::layout::stable_order`]: the handle's protocol id,
    /// unique while the window lives.
    pub fn key(&self) -> u32 {
        self.handle.id().protocol_id()
    }
}

/// The `state` array: native-endian `u32`s, `minimized` = 1, `activated` = 2.
fn states(raw: &[u8]) -> (bool, bool) {
    let mut activated = false;
    let mut minimized = false;
    let (values, _) = raw.as_chunks::<4>();
    for v in values {
        match u32::from_ne_bytes(*v) {
            1 => minimized = true,
            2 => activated = true,
            _ => {}
        }
    }
    (activated, minimized)
}

#[cfg(test)]
mod tests {
    use super::states;

    fn raw(values: &[u32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_ne_bytes()).collect()
    }

    #[test]
    fn the_state_array_says_which_window_is_in_front_and_which_is_folded() {
        assert_eq!(states(&raw(&[])), (false, false));
        assert_eq!(states(&raw(&[2])), (true, false));
        assert_eq!(states(&raw(&[0, 1])), (false, true), "maximized and folded");
        assert_eq!(states(&raw(&[3, 2, 0])), (true, false));
        // A truncated array is the compositor's bug; what can be read is used.
        let mut bad = raw(&[2]);
        bad.push(1);
        assert_eq!(states(&bad), (true, false));
    }
}
