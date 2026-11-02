#!/usr/bin/env python3
"""A stand-in desktop notification service for a guest without a desktop.

Takes the session-bus name org.freedesktop.Notifications and prints every message
it is asked to show, one JSON line each, the way sni-watcher.py does for the icon.
Needs python3-dbus and python3-gi.
"""
import json
import sys

import dbus
import dbus.mainloop.glib
import dbus.service
from gi.repository import GLib

IFACE = 'org.freedesktop.Notifications'

dbus.mainloop.glib.DBusGMainLoop(set_as_default=True)
bus = dbus.SessionBus()
name = dbus.service.BusName(IFACE, bus)


class Notifications(dbus.service.Object):
    def __init__(self):
        super().__init__(bus, '/org/freedesktop/Notifications')
        self.shown = 0

    @dbus.service.method(IFACE, in_signature='susssasa{sv}i', out_signature='u')
    def Notify(self, app, replaces, icon, summary, body, actions, hints, timeout):
        self.shown += 1
        print(json.dumps({
            'app': str(app), 'icon': str(icon), 'summary': str(summary), 'body': str(body),
            'desktop_entry': str(hints.get('desktop-entry', '')), 'timeout': int(timeout),
        }, ensure_ascii=False), flush=True)
        return dbus.UInt32(self.shown)

    @dbus.service.method(IFACE, out_signature='as')
    def GetCapabilities(self):
        return dbus.Array(['body'], signature='s')

    @dbus.service.method(IFACE, out_signature='ssss')
    def GetServerInformation(self):
        return ('mock', 'own-backup', '1', '1.2')


Notifications()
print('notifications up', flush=True)
sys.stdout.flush()
GLib.MainLoop().run()
