#!/usr/bin/env python3
"""A stand-in StatusNotifierWatcher for a guest without a desktop.

KDE Plasma (and GNOME's AppIndicator extension) own the session-bus name
org.kde.StatusNotifierWatcher; a tray icon registers its item with it. The
stand has no desktop, so this one takes the name and prints every item that
registers — which is what tells the icon really is in the notification area
protocol, not only that its process runs — and the labels of its menu
(com.canonical.dbusmenu). With --click LABEL it then clicks that menu item,
as a person would. Needs python3-dbus and python3-gi.

    sni-watcher.py [--click 'Открыть Own Backup']
"""
import argparse
import sys

import dbus
import dbus.mainloop.glib
import dbus.service
from gi.repository import GLib

IFACE = 'org.kde.StatusNotifierWatcher'

parser = argparse.ArgumentParser()
parser.add_argument('--click', help='menu item label to click once the item registers')
args = parser.parse_args()

dbus.mainloop.glib.DBusGMainLoop(set_as_default=True)
bus = dbus.SessionBus()
name = dbus.service.BusName(IFACE, bus)


class Watcher(dbus.service.Object):
    def __init__(self):
        super().__init__(bus, '/StatusNotifierWatcher')
        self.items = []

    @dbus.service.method(IFACE, in_signature='s', sender_keyword='sender')
    def RegisterStatusNotifierItem(self, service, sender=None):
        self.items.append(f'{sender}{service}' if service.startswith('/') else service)
        print('registered', sender, service, flush=True)
        self.StatusNotifierItemRegistered(service)
        path = service if service.startswith('/') else '/StatusNotifierItem'
        owner = sender if service.startswith('/') else service
        GLib.timeout_add(1000, lambda: show_menu(owner, path) and False)

    @dbus.service.method(IFACE, in_signature='s')
    def RegisterStatusNotifierHost(self, service):
        pass

    @dbus.service.signal(IFACE, signature='s')
    def StatusNotifierItemRegistered(self, service):
        pass

    def props(self):
        return {
            'IsStatusNotifierHostRegistered': True,
            'ProtocolVersion': dbus.Int32(0),
            'RegisteredStatusNotifierItems': dbus.Array(self.items, signature='s'),
        }

    @dbus.service.method(dbus.PROPERTIES_IFACE, in_signature='ss', out_signature='v')
    def Get(self, interface, prop):
        return self.props()[prop]

    @dbus.service.method(dbus.PROPERTIES_IFACE, in_signature='s', out_signature='a{sv}')
    def GetAll(self, interface):
        return self.props()


def show_menu(owner, path):
    """Print the item's menu labels; click --click among them."""
    try:
        item = bus.get_object(owner, path)
        menu_path = item.Get('org.kde.StatusNotifierItem', 'Menu',
                             dbus_interface=dbus.PROPERTIES_IFACE)
        menu = dbus.Interface(bus.get_object(owner, menu_path), 'com.canonical.dbusmenu')
        _, (_, _, children) = menu.GetLayout(0, -1, dbus.Array([], signature='s'))
        for child in children:
            ident, props, _ = child
            label = str(props.get('label', '')).replace('_', '')
            kind = str(props.get('type', 'standard'))
            print('menu', int(ident), kind, label, flush=True)
            if args.click and label == args.click:
                menu.Event(ident, 'clicked', dbus.Int32(0, variant_level=1),
                           dbus.UInt32(0))
                print('clicked', label, flush=True)
    except dbus.DBusException as e:
        print('menu error', e, flush=True)
    return False


Watcher()
print('watcher up', flush=True)
sys.stdout.flush()
GLib.MainLoop().run()
