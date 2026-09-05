// Vortex Live Activities — GNOME Shell extension.
//
// Draws a menu-bar pill (app icon + status) per active live activity,
// expanding on click to a card (status / detail / progress bar).
// Data comes from the Vortex daemon over D-Bus: org.vortex.LiveActivities,
// property `Activities` (JSON array), with PropertiesChanged on every update.

import St from 'gi://St';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Clutter from 'gi://Clutter';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PanelMenu from 'resource:///org/gnome/shell/ui/panelMenu.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const BUS_NAME = 'org.vortex.LiveActivities';
const OBJ_PATH = '/org/vortex/LiveActivities';
const IFACE = 'org.vortex.LiveActivities1';

// Universal Control: the app flips CursorHidden while the laptop cursor is
// "on the phone", so we hide the local pointer (the InputCapture portal can't).
const UC_NAME = 'org.vortex.UniversalControl';
const UC_PATH = '/org/vortex/UniversalControl';
const UC_IFACE = 'org.vortex.UniversalControl1';

// Extend mode (phone as a second screen) needs to draw the pointer into the
// stream itself: Mutter will not composite a cursor into a VIRTUAL monitor's
// screen cast — asking it to tears the session down after half a minute — so
// the app overlays one, and needs to know where the pointer is.
//
// The shell is the right place to answer that. `global.get_pointer()` is
// already in Mutter's logical coordinates, the same space the monitor layout
// uses, so the answer is a subtraction. Doing it from outside would mean
// undoing XWayland's fractional-scaling transform by hand.
const SHELL_NAME = 'org.vortex.Shell';
const SHELL_PATH = '/org/vortex/Shell';
const SHELL_XML = `
<node>
  <interface name="org.vortex.Shell1">
    <method name="GetVirtualPointer">
      <arg type="i" name="x" direction="out"/>
      <arg type="i" name="y" direction="out"/>
      <arg type="b" name="on" direction="out"/>
    </method>
    <method name="ActivateWindow">
      <arg type="s" name="wm_class" direction="in"/>
      <arg type="b" name="ok" direction="out"/>
    </method>
  </interface>
</node>`;

export default class VortexLiveExtension extends Extension {
    enable() {
        this._buttons = new Map(); // key -> PanelMenu.Button

        // Re-create the proxy whenever the daemon (re)appears; tear pills down
        // when it goes away.
        this._nameWatch = Gio.bus_watch_name(
            Gio.BusType.SESSION, BUS_NAME, Gio.BusNameWatcherFlags.NONE,
            () => this._connect(),
            () => { this._disconnect(); this._clearAll(); },
        );

        // Universal Control cursor-hide.
        this._cursorHidden = false;
        this._ucWatch = Gio.bus_watch_name(
            Gio.BusType.SESSION, UC_NAME, Gio.BusNameWatcherFlags.NONE,
            () => this._ucConnect(),
            () => { this._ucDisconnect(); this._showCursor(); },
        );

        this._shellExport();
    }

    disable() {
        if (this._nameWatch) { Gio.bus_unwatch_name(this._nameWatch); this._nameWatch = 0; }
        this._disconnect();
        this._clearAll();
        if (this._ucWatch) { Gio.bus_unwatch_name(this._ucWatch); this._ucWatch = 0; }
        this._ucDisconnect();
        this._showCursor(); // never leave the pointer inhibited on teardown
        this._shellUnexport();
    }

    // ── Pointer position for extend mode ────────────────────────────────────
    _shellExport() {
        try {
            this._shellImpl = Gio.DBusExportedObject.wrapJSObject(SHELL_XML, this);
            this._shellImpl.export(Gio.DBus.session, SHELL_PATH);
            this._shellOwnId = Gio.bus_own_name(
                Gio.BusType.SESSION, SHELL_NAME, Gio.BusNameOwnerFlags.NONE,
                null, null, null);
        } catch (e) {
            logError(e, 'vortex-live: shell iface');
        }
    }

    _shellUnexport() {
        if (this._shellOwnId) { Gio.bus_unown_name(this._shellOwnId); this._shellOwnId = 0; }
        if (this._shellImpl) {
            try { this._shellImpl.unexport(); } catch (e) {}
            this._shellImpl = null;
        }
    }

    /** Pointer position relative to the virtual monitor's top-left, and whether
     *  it is actually on it.
     *
     *  Mutter is asked which of its monitors is the virtual one, rather than
     *  guessing from a name: `Main.layoutManager.monitors` carries only index,
     *  position, size and scale — no connector — so the older name test read
     *  `undefined`, skipped every monitor and answered "not on it" forever. That
     *  is a cursor that never appears on the phone, since this answer is the only
     *  thing that positions it.
     *
     *  The layout is still where the RECT comes from: `get_monitor_geometry` is
     *  in the same logical coordinates as `get_pointer`, which is the whole
     *  reason this lives in the shell. */
    GetVirtualPointer() {
        try {
            const mm = global.backend?.get_monitor_manager?.();
            const monitors = mm?.get_monitors?.() ?? [];
            for (const mon of monitors) {
                if (!mon.is_virtual?.()) continue;
                const idx = mm.get_monitor_for_connector(mon.get_connector());
                if (idx < 0) continue;
                const r = global.display.get_monitor_geometry(idx);
                const [px, py] = global.get_pointer();
                const on = px >= r.x && px < r.x + r.width &&
                           py >= r.y && py < r.y + r.height;
                return [px - r.x, py - r.y, on];
            }
        } catch (e) {
            logError(e, 'vortex-live: pointer');
        }
        return [0, 0, false];
    }

    /**
     * Bring the app's window to the front, from INSIDE the compositor.
     *
     * Nothing the app can do from outside achieves this on a Wayland session.
     * Wayland has no "raise me" for ordinary clients by design — the only
     * sanctioned route is an xdg-activation token, and a token is issued for a
     * real user action delivered TO that app. A tray click goes to the shell,
     * not to Vortex, and the appindicator protocol carries no token, so the app
     * has nothing to present.
     *
     * The X11 route does not help either, even though the app runs on
     * XWayland: `_NET_ACTIVE_WINDOW` arbitrates the X stacking order, and on
     * this desktop Vortex is the ONLY X client — every other window is a native
     * Wayland one that EWMH cannot reach. Mutter answers the request with its
     * focus-stealing policy instead, which is the "Vortex is ready" notice
     * users see while the window stays behind the terminal.
     *
     * An extension runs inside Mutter, so `activate()` here is the compositor
     * raising its own window: no policy stands between the request and the
     * result. This is why the shell extension — already installed for the live
     * pills — is the right place for it.
     */
    ActivateWindow(wmClass) {
        try {
            const want = (wmClass || '').toLowerCase();
            const now = global.get_current_time();
            for (const actor of global.get_window_actors()) {
                const w = actor.meta_window;
                if (!w) continue;
                const cls = (w.get_wm_class() || '').toLowerCase();
                const inst = (w.get_wm_class_instance() || '').toLowerCase();
                if (cls !== want && inst !== want) continue;
                // Follow it to its workspace rather than yanking the window
                // across — the same thing clicking it in the overview does.
                const ws = w.get_workspace();
                if (ws && ws !== global.workspace_manager.get_active_workspace()) {
                    ws.activate(now);
                }
                if (w.minimized) w.unminimize();
                w.activate(now);
                return true;
            }
        } catch (e) {
            logError(e, 'vortex-live: activate');
        }
        return false;
    }

    _ucConnect() {
        if (this._ucProxy) return;
        try {
            this._ucProxy = Gio.DBusProxy.new_for_bus_sync(
                Gio.BusType.SESSION, Gio.DBusProxyFlags.NONE, null,
                UC_NAME, UC_PATH, UC_IFACE, null);
            this._ucChangedId = this._ucProxy.connect(
                'g-properties-changed', () => this._ucRefresh());
            this._ucRefresh();
        } catch (e) {
            logError(e, 'vortex-live: uc proxy');
        }
    }

    _ucDisconnect() {
        if (this._ucProxy && this._ucChangedId) {
            try { this._ucProxy.disconnect(this._ucChangedId); } catch (e) {}
        }
        this._ucChangedId = 0;
        this._ucProxy = null;
    }

    _ucRefresh() {
        if (!this._ucProxy) return;
        // The cached property is the fast path, but it is only as good as the
        // PropertiesChanged that filled it. Ask the app directly when the cache
        // has nothing — otherwise a single missed signal leaves the pointer
        // visible for the whole time it is supposed to be on the phone, which
        // is indistinguishable from the feature being broken.
        let hide = null;
        const v = this._ucProxy.get_cached_property('CursorHidden');
        if (v) hide = v.deepUnpack();
        if (hide === null) hide = this._ucReadProperty();
        console.log(`vortex-uc: CursorHidden=${hide} (cached=${v ? 'yes' : 'no'})`);
        if (hide) this._hideCursor(); else this._showCursor();
    }

    /** Read CursorHidden straight off the bus. Returns false if it can't. */
    _ucReadProperty() {
        try {
            const r = this._ucProxy.get_connection().call_sync(
                UC_NAME, UC_PATH, 'org.freedesktop.DBus.Properties', 'Get',
                new GLib.Variant('(ss)', [UC_IFACE, 'CursorHidden']),
                new GLib.VariantType('(v)'), Gio.DBusCallFlags.NONE, 1000, null);
            return r.deepUnpack()[0].deepUnpack();
        } catch (e) {
            logError(e, 'vortex-live: uc property read');
            return false;
        }
    }

    /** Mutter's cursor tracker, whichever way this shell version exposes it. */
    _cursorTracker() {
        if (global.backend?.get_cursor_tracker)
            return global.backend.get_cursor_tracker();
        return global.display?.get_cursor_tracker?.() ?? null;
    }

    _seat() {
        try { return Clutter.get_default_backend().get_default_seat(); }
        catch (e) { return null; }
    }

    // inhibit_cursor_visibility() is the API GNOME itself recommends from 49
    // onwards; the older set_pointer_visible(false) loses the cursor again the
    // moment the mouse moves. The inhibitor is REF-COUNTED, so hide and show
    // have to stay balanced — hence the flag.
    //
    // The unfocus inhibit is what the hide-cursor extension pairs it with, and
    // it is not decoration: without it the seat can drop pointer focus while the
    // cursor is hidden.
    _hideCursor() {
        if (this._cursorHidden) return;
        try {
            const t = this._cursorTracker();
            if (!t) { console.log('vortex-uc: no cursor tracker'); return; }
            // Said plainly, because the symptom is a cursor on each screen and
            // nothing else to explain it. The older set_pointer_visible(false) is
            // not a fallback worth having — it loses the cursor again on the next
            // motion event, which during a crossing is immediately.
            if (!t.inhibit_cursor_visibility) {
                console.log('vortex-uc: this GNOME has no inhibit_cursor_visibility '
                    + '(needs 49+) — the laptop cursor will stay visible');
                return;
            }
            const before = t.get_pointer_visible?.();
            t.inhibit_cursor_visibility();
            this._seat()?.inhibit_unfocus();
            this._cursorHidden = true;
            // These two numbers are the whole diagnosis. true -> false and a
            // cursor still on screen means Mutter is drawing it from somewhere
            // the tracker does not govern (it holds the pointer at the barrier
            // during an input-capture session) — which is a compositor gap, not
            // ours. Staying true means the inhibit never took at all.
            console.log(`vortex-uc: hide, pointer_visible ${before} -> ${t.get_pointer_visible?.()}`);
        } catch (e) { logError(e, 'vortex-live: hide cursor'); }
    }

    _showCursor() {
        if (!this._cursorHidden) return;
        try {
            this._cursorTracker()?.uninhibit_cursor_visibility();
            this._seat()?.uninhibit_unfocus();
            console.log('vortex-uc: show');
        } catch (e) { logError(e, 'vortex-live: show cursor'); }
        this._cursorHidden = false;
    }

    _connect() {
        if (this._proxy) return;
        try {
            this._proxy = Gio.DBusProxy.new_for_bus_sync(
                Gio.BusType.SESSION, Gio.DBusProxyFlags.NONE, null,
                BUS_NAME, OBJ_PATH, IFACE, null);
            this._changedId = this._proxy.connect(
                'g-properties-changed', () => this._refresh());
            this._refresh();
        } catch (e) {
            logError(e, 'vortex-live: proxy');
        }
    }

    _disconnect() {
        if (this._proxy && this._changedId) {
            try { this._proxy.disconnect(this._changedId); } catch (e) {}
        }
        this._changedId = 0;
        this._proxy = null;
    }

    _clearAll() {
        if (!this._buttons) return;
        for (const btn of this._buttons.values()) { this._stopTimer(btn); btn.destroy(); }
        this._buttons.clear();
    }

    _refresh() {
        if (!this._proxy) return;
        let json = '[]';
        const v = this._proxy.get_cached_property('Activities');
        if (v) json = v.deepUnpack();
        let list;
        try { list = JSON.parse(json); } catch (e) { list = []; }

        const seen = new Set();
        for (const a of list) {
            if (!a || !a.key) continue;
            seen.add(a.key);
            this._upsert(a);
        }
        for (const [key, btn] of [...this._buttons]) {
            if (!seen.has(key)) { this._stopTimer(btn); btn.destroy(); this._buttons.delete(key); }
        }
    }

    _upsert(a) {
        let btn = this._buttons.get(a.key);
        if (!btn) {
            // menuAlignment 0.5 → the popover arrow sits at the CENTER of the
            // pill, so the expanded card opens centered under its trigger
            // (0.0 anchored the arrow to the pill's left edge, pushing the whole
            // card off to the right).
            btn = new PanelMenu.Button(0.5, 'vortex-live', false);

            // --- panel pill: icon + short status -----------------------------
            const pill = new St.BoxLayout({style_class: 'vortex-pill'});
            btn._icon = new St.Icon({style_class: 'system-status-icon'});
            btn._label = new St.Label({y_align: Clutter.ActorAlign.CENTER, style_class: 'vortex-pill-label'});
            pill.add_child(btn._icon);
            pill.add_child(btn._label);
            btn.add_child(pill);

            // Handoff pill: ONE click opens the page in the default browser, with
            // NO menu/card. Clicking a PanelMenu.Button calls menu.open(), which
            // takes a modal input grab (the "invisible curtain"). So we OVERRIDE
            // open() for the handoff pill to launch the URL instead of ever
            // opening the menu — no grab. `btn._url` (the http(s) URL) is set only
            // for the handoff pill, in the content update below; null otherwise.
            btn._url = null;
            const _origMenuOpen = btn.menu.open.bind(btn.menu);
            btn.menu.open = (animate) => {
                if (btn._url) {
                    try { Gio.AppInfo.launch_default_for_uri(btn._url, null); }
                    catch (e) { logError(e); }
                    return; // never open the menu → no modal grab, no curtain
                }
                _origMenuOpen(animate);
            };

            // --- expanded card -----------------------------------------------
            const card = new St.BoxLayout({vertical: true, style_class: 'vortex-card'});
            const head = new St.BoxLayout({style_class: 'vortex-card-head'});
            btn._cardIcon = new St.Icon({icon_size: 22});
            btn._app = new St.Label({style_class: 'vortex-app', y_align: Clutter.ActorAlign.CENTER});
            head.add_child(btn._cardIcon);
            head.add_child(btn._app);
            btn._title = new St.Label({style_class: 'vortex-title'});
            btn._text = new St.Label({style_class: 'vortex-text'});

            btn._progress = -1;
            btn._bar = new St.DrawingArea({style_class: 'vortex-bar'});
            btn._bar.connect('repaint', (area) => this._drawBar(area, btn._progress));

            btn._sub = new St.Label({style_class: 'vortex-sub'});

            card.add_child(head);
            card.add_child(btn._title);
            card.add_child(btn._text);
            card.add_child(btn._bar);
            card.add_child(btn._sub);

            // In-call action buttons — shown only for the call pill
            // (key 'vortex-call'); clicking sends CallAction(verb) to the
            // daemon → the phone. Labels + verbs are DYNAMIC (set in the
            // content update below): Mute↔Unmute, Speaker on/off, and Speaker
            // is hidden when wireless earbuds are connected.
            btn._callRow = new St.BoxLayout({style_class: 'vortex-call-actions'});
            const mkBtn = () => {
                const b = new St.Button({
                    style_class: 'vortex-call-btn', x_expand: true, can_focus: true,
                });
                b._verb = '';
                b.connect('clicked', () => {
                    if (b._verb) this._callAction(b._verb);
                    btn.menu.close();
                });
                return b;
            };
            btn._muteBtn = mkBtn();
            btn._speakerBtn = mkBtn();
            btn._endBtn = mkBtn();
            btn._endBtn._verb = 'end';
            btn._endBtn.label = 'End';
            btn._callRow.add_child(btn._muteBtn);
            btn._callRow.add_child(btn._speakerBtn);
            btn._callRow.add_child(btn._endBtn);
            card.add_child(btn._callRow);

            // Now-playing transport buttons — shown only for a media pill
            // (activity carries a `playing` flag). Rides the same CallAction
            // channel; the verb carries the player's package name so the
            // phone controls the right session. Unlike the call buttons the
            // card stays OPEN — next/prev are often pressed repeatedly.
            btn._mediaRow = new St.BoxLayout({style_class: 'vortex-call-actions'});
            const mkMediaBtn = (label) => {
                const b = new St.Button({
                    style_class: 'vortex-call-btn', x_expand: true, can_focus: true,
                    label,
                });
                b._verb = '';
                b.connect('clicked', () => { if (b._verb) this._callAction(b._verb); });
                return b;
            };
            btn._prevBtn = mkMediaBtn('⏮');
            btn._playBtn = mkMediaBtn('▶');
            btn._nextBtn = mkMediaBtn('⏭');
            btn._mediaRow.add_child(btn._prevBtn);
            btn._mediaRow.add_child(btn._playBtn);
            btn._mediaRow.add_child(btn._nextBtn);
            card.add_child(btn._mediaRow);

            btn.menu.box.add_child(card);

            // Right box → the pill sits among the system-tray indicators (next
            // to Vortex's own tray icon), not in front of the clock.
            Main.panel.addToStatusArea('vortex-live-' + a.key, btn, 0, 'right');
            this._buttons.set(a.key, btn);
        }

        // --- content -----------------------------------------------------------
        if (a.icon) {
            const g = Gio.icon_new_for_string(a.icon);
            btn._icon.gicon = g;
            btn._cardIcon.gicon = g;
        }
        btn._app.text = a.app || '';
        btn._title.text = a.title || '';
        btn._sub.text = a.sub || '';
        btn._sub.visible = !!(a.sub && a.sub.length);
        btn._progress = (typeof a.progress === 'number') ? a.progress : -1;
        btn._bar.visible = btn._progress >= 0;
        btn._title.visible = !!(a.title && a.title.length);
        // Handoff pill: clicking it opens the page (URL carried in `sub`).
        btn._url = (a.key === 'vortex-handoff' && a.sub) ? a.sub : null;
        // In-call pill action buttons: dynamic from the call audio state.
        btn._callRow.visible = (a.key === 'vortex-call');
        if (a.key === 'vortex-call') {
            // Mute ↔ Unmute toggle.
            const muted = !!a.muted;
            btn._muteBtn.label = muted ? 'Unmute' : 'Mute';
            btn._muteBtn._verb = muted ? 'unmute' : 'mute';
            // Speaker: hidden when earbuds are connected; otherwise on/off.
            const speaker = !!a.speaker;
            btn._speakerBtn.visible = !a.has_earbuds;
            btn._speakerBtn.label = speaker ? 'Speaker off' : 'Speaker';
            btn._speakerBtn._verb = speaker ? 'speaker_off' : 'speaker_on';
        }
        // Now-playing pill: transport row + ⏸/▶ from the live playing flag.
        const isMedia = (a.playing !== undefined && a.playing !== null);
        btn._mediaRow.visible = isMedia;
        if (isMedia) {
            const appId = a.app_id || '';
            btn._playBtn.label = a.playing ? '⏸' : '▶';
            btn._playBtn._verb = 'media_play_pause:' + appId;
            btn._prevBtn._verb = 'media_prev:' + appId;
            btn._nextBtn._verb = 'media_next:' + appId;
        }
        btn._bar.queue_repaint();

        // Duration timer (the in-call pill): the daemon sends `started_at`
        // (epoch-ms the call connected) ONCE and we tick the label LOCALLY,
        // so the daemon needn't republish every second (that starved its
        // D-Bus method dispatch). `started_at` 0 / absent → static text.
        btn._baseText = a.text || '';
        if (typeof a.started_at === 'number' && a.started_at > 0) {
            btn._startedAt = a.started_at;
            this._renderTimed(btn);
            if (!btn._timerId) {
                btn._timerId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 1000, () => {
                    this._renderTimed(btn);
                    return GLib.SOURCE_CONTINUE;
                });
            }
        } else {
            this._stopTimer(btn);
            // Media pill leads with the TRACK (title); others with the detail.
            const status = (isMedia
                ? (a.title || a.text || a.app || '')
                : (a.text || a.title || a.app || '')).slice(0, 28);
            btn._label.text = ' ' + status;
            btn._text.text = a.text || '';
            btn._text.visible = !!(a.text && a.text.length);
        }
    }

    // Update a timed pill's label/card to "<base> · M:SS" from started_at.
    _renderTimed(btn) {
        const secs = Math.max(0, Math.floor((GLib.get_real_time() / 1000 - btn._startedAt) / 1000));
        const h = Math.floor(secs / 3600), m = Math.floor((secs % 3600) / 60), s = secs % 60;
        const pad = (n) => (n < 10 ? '0' + n : '' + n);
        const dur = h > 0 ? `${h}:${pad(m)}:${pad(s)}` : `${m}:${pad(s)}`;
        const txt = btn._baseText ? `${btn._baseText} · ${dur}` : dur;
        btn._label.text = ' ' + txt.slice(0, 28);
        btn._text.text = txt;
        btn._text.visible = true;
    }

    _stopTimer(btn) {
        if (btn._timerId) {
            GLib.Source.remove(btn._timerId);
            btn._timerId = 0;
        }
    }

    // Invoke an in-call action on the daemon (Mute / Speaker / End) → phone.
    _callAction(verb) {
        if (!this._proxy) return;
        try {
            this._proxy.call('CallAction', new GLib.Variant('(s)', [verb]),
                Gio.DBusCallFlags.NONE, -1, null, null);
        } catch (e) {
            logError(e, 'vortex-live: CallAction');
        }
    }

    _drawBar(area, progress) {
        const cr = area.get_context();
        const [w, h] = area.get_surface_size();
        const r = h / 2;
        const rr = (x0, y0, x1, y1) => {
            cr.newSubPath();
            cr.arc(x1 - r, y0 + r, r, -Math.PI / 2, 0);
            cr.arc(x1 - r, y1 - r, r, 0, Math.PI / 2);
            cr.arc(x0 + r, y1 - r, r, Math.PI / 2, Math.PI);
            cr.arc(x0 + r, y0 + r, r, Math.PI, 1.5 * Math.PI);
            cr.closePath();
        };
        // track
        cr.setSourceRGBA(1, 1, 1, 0.16);
        rr(0, 0, w, h);
        cr.fill();
        // fill (accent green)
        const p = Math.max(0, Math.min(100, progress));
        const fw = Math.max(h, (w * p) / 100);
        cr.setSourceRGBA(0.20, 0.78, 0.35, 1.0);
        rr(0, 0, fw, h);
        cr.fill();
        cr.$dispose();
    }
}
