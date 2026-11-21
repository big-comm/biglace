// The clipboard for the BigLace app (org.communitybig.biglace): each copy
// goes to the app, which hands it to the phone; what the phone copies comes
// back as the app's Set signal, sent to this extension alone.

import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Meta from 'gi://Meta';
import St from 'gi://St';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const APP = 'org.communitybig.biglace';
const PATH = '/org/communitybig/biglace/Clipboard';
const INTERFACE = 'org.communitybig.biglace.Clipboard';
const PNG = 'image/png';
const TEXT = 'text/plain;charset=utf-8';

export default class BigLaceExtension extends Extension {
    enable() {
        this._clipboard = St.Clipboard.get_default();
        this._selection = global.display.get_selection();
        this._ownerChanged = this._selection.connect('owner-changed', (_selection, type) => {
            if (type === Meta.SelectionType.SELECTION_CLIPBOARD && this._appRunning)
                this._copied();
        });
        // The app may start after the shell, or restart: attach each time it appears.
        this._watch = Gio.bus_watch_name(Gio.BusType.SESSION, APP, Gio.BusNameWatcherFlags.NONE,
            () => {
                this._appRunning = true;
                this._call('Attach', null);
            },
            () => {
                this._appRunning = false;
            });
        this._subscription = Gio.DBus.session.signal_subscribe(APP, INTERFACE, 'Set', PATH, null,
            Gio.DBusSignalFlags.NONE, (_conn, _sender, _path, _iface, _signal, params) => this._set(params));
    }

    disable() {
        this._selection.disconnect(this._ownerChanged);
        Gio.bus_unwatch_name(this._watch);
        Gio.DBus.session.signal_unsubscribe(this._subscription);
        this._clipboard = null;
        this._selection = null;
    }

    _call(method, params) {
        Gio.DBus.session.call(APP, PATH, INTERFACE, method, params, null,
            Gio.DBusCallFlags.NO_AUTO_START, -1, null, (conn, result) => {
                try {
                    conn.call_finish(result);
                } catch (e) {
                    // The app quit meanwhile: it attaches again when it comes back.
                    if (!e.matches(Gio.DBusError, Gio.DBusError.SERVICE_UNKNOWN))
                        logError(e, `BigLace: ${method}`);
                }
            });
    }

    _copied() {
        const types = this._clipboard.get_mimetypes(St.ClipboardType.CLIPBOARD);
        if (types.includes(PNG)) {
            this._clipboard.get_content(St.ClipboardType.CLIPBOARD, PNG, (_clipboard, bytes) => {
                if (bytes && bytes.get_size() > 0)
                    this._call('Copied', new GLib.Variant('(say)', [PNG, bytes.get_data()]));
            });
        } else {
            this._clipboard.get_text(St.ClipboardType.CLIPBOARD, (_clipboard, text) => {
                if (text)
                    this._call('Copied', new GLib.Variant('(say)', [TEXT, new TextEncoder().encode(text)]));
            });
        }
    }

    _set(params) {
        const [mime, data] = params.deep_unpack();
        if (mime === PNG)
            this._clipboard.set_content(St.ClipboardType.CLIPBOARD, PNG, new GLib.Bytes(data));
        else
            this._clipboard.set_text(St.ClipboardType.CLIPBOARD, new TextDecoder().decode(data));
    }
}
