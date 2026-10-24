//! The calls of libgtk-layer-shell the strip makes, declared here.
//!
//! The library itself is maintained and packaged (Debian and Ubuntu:
//! `libgtk-layer-shell0`); its Rust bindings for GTK 3 (`gtk-layer-shell`,
//! `gtk-layer-shell-sys`) are archived upstream (RUSTSEC-2024-0422/0423) with
//! no GTK 3 successor, and stopped the release audit. Seven functions are not
//! worth an unmaintained dependency. Signatures and enum values are those of
//! `gtk-layer-shell.h` (0.6 and later).

use std::ffi::{c_char, c_int, CString};

use gtk::glib::translate::ToGlibPtr;
use gtk::prelude::*;

type GtkWindow = gtk::ffi::GtkWindow;
type Gboolean = gtk::glib::ffi::gboolean;

#[link(name = "gtk-layer-shell")]
extern "C" {
    fn gtk_layer_is_supported() -> Gboolean;
    fn gtk_layer_init_for_window(window: *mut GtkWindow);
    fn gtk_layer_set_namespace(window: *mut GtkWindow, name_space: *const c_char);
    fn gtk_layer_set_layer(window: *mut GtkWindow, layer: c_int);
    fn gtk_layer_set_anchor(window: *mut GtkWindow, edge: c_int, anchor_to_edge: Gboolean);
    fn gtk_layer_set_exclusive_zone(window: *mut GtkWindow, exclusive_zone: c_int);
    fn gtk_layer_auto_exclusive_zone_enable(window: *mut GtkWindow);
}

/// `GtkLayerShellLayer`, the values the strip uses.
#[derive(Debug, Clone, Copy)]
pub enum Layer {
    Background = 0,
    Top = 2,
}

/// `GtkLayerShellEdge`.
#[derive(Debug, Clone, Copy)]
pub enum Edge {
    Left = 0,
    Right = 1,
    Top = 2,
    Bottom = 3,
}

/// Does the compositor offer the layer-shell protocol at all?
pub fn is_supported() -> bool {
    // SAFETY: no arguments; GTK is initialised by the caller (`run`).
    unsafe { gtk_layer_is_supported() != 0 }
}

/// A window on a layer-shell surface — the methods of the archived crate the
/// strip used, under the same names.
pub trait LayerShell {
    fn init_layer_shell(&self);
    fn set_namespace(&self, name: &str);
    fn set_layer(&self, layer: Layer);
    fn set_anchor(&self, edge: Edge, anchor: bool);
    fn set_exclusive_zone(&self, zone: i32);
    fn auto_exclusive_zone_enable(&self);
}

impl<W: IsA<gtk::Window>> LayerShell for W {
    fn init_layer_shell(&self) {
        // SAFETY: a live GtkWindow, not yet realized (the library requires
        // that, and every caller sets the surface up before `show_all`).
        unsafe { gtk_layer_init_for_window(window(self)) }
    }

    fn set_namespace(&self, name: &str) {
        let name = CString::new(name).expect("a layer namespace has no NUL");
        // SAFETY: a live window; the library copies the string.
        unsafe { gtk_layer_set_namespace(window(self), name.as_ptr()) }
    }

    fn set_layer(&self, layer: Layer) {
        // SAFETY: a live window, a value of the C enum.
        unsafe { gtk_layer_set_layer(window(self), layer as c_int) }
    }

    fn set_anchor(&self, edge: Edge, anchor: bool) {
        // SAFETY: a live window, a value of the C enum.
        unsafe { gtk_layer_set_anchor(window(self), edge as c_int, Gboolean::from(anchor)) }
    }

    fn set_exclusive_zone(&self, zone: i32) {
        // SAFETY: a live window.
        unsafe { gtk_layer_set_exclusive_zone(window(self), zone) }
    }

    fn auto_exclusive_zone_enable(&self) {
        // SAFETY: a live window.
        unsafe { gtk_layer_auto_exclusive_zone_enable(window(self)) }
    }
}

fn window<W: IsA<gtk::Window>>(w: &W) -> *mut GtkWindow {
    w.as_ref().to_glib_none().0
}
