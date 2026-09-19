<p align="center">
  <img src="linux/ui-tauri/src/assets/vortex_logo.png" width="128" alt="Vortex logo">
</p>

<h1 align="center">Vortex</h1>

<p align="center">
  <b>Your devices, one flow.</b><br>
  A seamless ecosystem for Linux and Android.
</p>

<p align="center">
  <img src="https://img.shields.io/badge/status-beta-brightgreen" alt="Status">
  <img src="https://img.shields.io/badge/version-1.0.0--beta-blue" alt="Version">
  <img src="https://img.shields.io/badge/platform-Linux%20%7C%20Android-blue" alt="Platform">
  <img src="https://img.shields.io/badge/license-GPLv3-blue" alt="License">
</p>

---

## 🧪 Status: Beta

V1 is **feature-complete** — everything listed below works and is in closed
beta. So far it has been live-tested on one device pair (Redmi 9 / MIUI +
Ubuntu / GNOME); widening that coverage is what this beta is for.

---

## 📖 What is this?

Vortex gives your Linux and Android devices the seamless one-ecosystem
feel — notifications, clipboard, calls, audio and files flowing between
them as if they were one device:

- ✅ **Open** — any Android + any Linux
- ✅ **Offline-first** — no cloud, works with no internet at all
- ✅ **Hardware-agnostic** — no premium gear required (budget BT earbuds work)
- ✅ **Privacy-first** — everything end-to-end encrypted, stored locally

## 🎯 Features (working today)

- **Notification mirror** — bidirectional phone↔laptop: dismiss-sync, action
  buttons, inline reply from the laptop
- **Universal clipboard** — text + images sync, with history (Super+V)
- **Smart audio handoff** — earbuds follow your music between devices;
  jump to the phone on a call and come back after
- **Phone companion** — call banner (accept/decline), SMS, contacts,
  recent calls, dialing from the laptop
- **Browsing handoff** — continue the page you're reading on the phone
  with one click on the laptop
- **Proximity lock** — laptop locks when you walk away with the phone,
  unlocks when you're back
- **Notes + To-dos** — one list on both devices, with due-date reminders
- **File sharing** — instant drop-style, Nautilus/Dolphin integration,
  Wi-Fi Direct fast path
- **SMS login codes** — a verification code arriving on the phone lands on the
  laptop clipboard, ready to paste
- **Emoji-SAS pairing** — easy AND secure: compare 3 emoji, done

### Screen features (need adb — see [step 6](#6-adb--only-for-the-screen-features))

- **Universal Control** — push the laptop's cursor off the screen edge and it
  arrives on the phone, driving the phone's own cursor. The keyboard follows it,
  and two-finger trackpad scrolling scrolls the phone. You pick which side the
  phone sits on, the way you arrange displays; there is a little resistance at
  the edge so you cannot cross by accident. Push back out the way you came in
  and control returns to the laptop. **Experimental:** besides adb it needs
  **Wayland** — holding the cursor at the screen edge goes through the
  input-capture portal, which the compositor provides. Tested on GNOME 45+;
  KDE Plasma 6.1+ and Hyprland ship that portal too (untested here), Sway/wlroots
  does not have it yet, and X11 has no such thing at all. Hiding the laptop's own
  cursor is GNOME-only for now.
- **Second screen** — the phone becomes a *real* extra monitor for the laptop:
  it appears in Displays and windows can be dragged onto it. GNOME only (it uses
  Mutter's own screen-cast API), and view-only — touches on the phone do not
  reach the laptop yet.
- **Screen mirroring** (both directions) and **continuity camera**
  (phone → Linux webcam) — experimental.

## 🛠 Tech stack

| Layer | Stack |
|---|---|
| Android | Kotlin + Jetpack Compose + platform BLE APIs |
| Linux daemon | Rust + Tokio + bluer + zbus |
| Linux GUI | Tauri 2.0 + Vue 3 + TypeScript |
| Shared protocol | Protocol Buffers + Noise Protocol |
| Transport | BLE (signaling) + LAN/Wi-Fi (bulk) |

## 🚀 Install

**Requirements:** Android 10+ phone · Linux with BlueZ (Ubuntu/Debian, Fedora,
Arch, openSUSE) · Bluetooth (BLE) on both devices. Build tools (Rust/Node) are
installed by the script itself. A Windows build is available too, cross-built
from Linux — see [step 7](#7-laptop-windows--experimental).

### 1. Laptop (Linux)

```bash
git clone https://github.com/zoir-dev/vortex && cd vortex
./install_linux.sh
```

One command does everything and **asks nothing** (auto-yes; pass `--ask` if
you want prompts): system libraries (single `sudo` step, distro auto-detected),
build (10–20 min the first time), install to `~/.local/bin`, menu shortcut,
**autostart** (sits in the tray after login), "Share via Vortex" in
Nautilus/Dolphin, and a top-bar "pill" extension on GNOME. Updates use the
same script. On GNOME+Wayland the pill appears after the first logout/login;
on non-GNOME desktops a tray icon is used instead.

**⚠ Secure Boot:** installation won't stop, but the *Experimental*
continuity-camera module (`v4l2loopback`) stays unsigned and may not load.
If you need it, run once: `sudo dpkg-reconfigure v4l2loopback-dkms` (set a
password, then enter it on the blue MOK screen at reboot). Everything else
is independent of this.

### 2. Phone (Android)

**Easiest — with a USB cable.** The one manual prerequisite is **USB
debugging** (Android security requires you to open that door yourself):

1. Settings → About phone → tap **Build number** **7 times** ("You are now
   a developer"; on Xiaomi: tap **MIUI version** 7 times).
2. Settings → System → **Developer options** → **USB debugging** → ON.
3. Plug the cable; accept **"Allow USB debugging?"** on the phone.

Then:

```bash
./install_android.sh
```

The script installs JDK/adb/Android SDK if needed (one `sudo`), builds,
installs to the phone, and enables Notification access + Accessibility +
background clipboard over adb by itself.

**APK route** (no cable/adb): install the release APK from
[Releases](https://github.com/zoir-dev/vortex/releases), then enable two
things manually: Settings → Apps → Special access → **Notification access**
and **Accessibility** → Vortex ✓. (The app also deep-links you to those
pages when needed.)

### 3. Permissions — the phone asks by itself

On first launch the app requests what it needs (Bluetooth/Nearby devices,
notifications, later calls/SMS/contacts) — **grant them all**; each unlocks
one feature. Skip any and the app will remind you later.

### 4. Autostart — the most important manual step! 📌

Many vendors kill background apps; without Autostart, Vortex won't come
back up after a reboot. The app shows a reminder card that opens the right
settings page — you just tick the box:

- **Xiaomi/Redmi/POCO:** Security → Autostart → Vortex ✓
- **Samsung:** Settings → Battery → Never sleeping apps → Vortex
- Other brands: look for "Autostart" / "battery optimization" exceptions in
  your vendor's settings

**Extra on Xiaomi:** without Developer options → **MIUI optimization → OFF**,
calling/SMS from the laptop won't work (MIUI silently blocks those permissions).

### 5. Pairing

1. On the laptop open Vortex (from the tray) → **"Add phone"**; keep the app
   open on the phone.
2. The radar finds the phone → click it → **3 emoji** appear on both screens —
   check they match (that's the security step).
3. Tap **Confirm** on the phone — done. No re-pairing after that; devices
   reconnect on their own (BLE or same Wi-Fi).

⚠ If another BLE phone-link app is running on the laptop, close it during
pairing — they fight over the BLE channel.

### 6. adb — only for the screen features

Everything above works without this. Universal Control, the second screen and
screen mirroring do not, and it is worth knowing why before you set it up.

Drawing a cursor on the phone, or typing into it, means writing to
`/dev/uinput`, and only the **shell** user can do that. adb is how a normal
Android device hands you that user. There is no root and no vendor account
involved — Vortex deliberately avoids the `INJECT_EVENTS` route that Xiaomi
gates behind "USB debugging (Security settings)" and a Mi account.

```bash
# once, with the cable plugged in: Developer options → USB debugging,
# then accept the key prompt on the phone
adb devices          # your phone should say "device", not "unauthorized"

# to go cable-free:
adb tcpip 5555       # then unplug
```

After that the laptop reconnects on its own — it remembers the phone's address
and re-dials it whenever adb has no device.

**What actually goes onto the phone.** Vortex pushes a 20 KB native helper to
`/data/local/tmp/vortex_inject` and runs it as the shell user; it is what opens
`/dev/uinput` and creates the virtual mouse, touchscreen and keyboard. It is not
installed, it holds no permissions of its own, and it dies with the session. The
source is [`android/inject/vortex_inject.c`](android/inject/vortex_inject.c) and
`android/inject/build.sh` rebuilds it — with NDK r26b that comes out byte-for-byte
identical to the committed binary, so you can check the copy in this repo instead
of taking our word for it:

```bash
OUT=/tmp/vi android/inject/build.sh
cmp /tmp/vi linux/ui-tauri/src-tauri/assets/vortex_inject && echo identical
```

⚠ **Android 10 and older:** `adb tcpip 5555` has to be redone over the cable
after every phone reboot. Android 11+ has proper Wireless debugging with its own
pairing, which survives reboots.

**"Configure physical keyboard"** — Android posts this whenever a keyboard is
attached that it has no layout for, and it has no way to save a layout for a
virtual one. Turning on **Settings → Physical keyboard → Show virtual keyboard**
lets Vortex keep one keyboard for the whole session, so you see the notification
once instead of on every crossing. The notification itself can also be switched
off: long-press it → turn off that category. Nothing else uses it.

### 7. Laptop (Windows) — experimental

There is no Windows installer yet. The app is **cross-built from Linux**: if
you already build the Linux app on this machine, one more script produces the
Windows `.exe` from the same checkout.

```bash
./install_windows.sh                 # toolchain + vortex-ui-tauri.exe
./install_windows.sh --installer     # also an NSIS setup.exe
```

It installs what is missing (clang/lld/llvm, the `x86_64-pc-windows-msvc` Rust
target, `cargo-xwin`, node) and builds. The first run additionally downloads
Microsoft's CRT and Windows SDK — a few hundred MB, once, into
`~/.cache/cargo-xwin`; `cargo-xwin` accepts Microsoft's licence for that
download on your behalf, so build on Windows instead if you would rather not.
The result lands in
`linux/ui-tauri/src-tauri/target/x86_64-pc-windows-msvc/release/`.

Copy the `.exe` to the Windows machine and run it — there is nothing to
install, it sits in the tray like the Linux build, and pairing is the same BLE
flow from the same phone app.

**What works:** pairing, reconnect, notification mirroring, clipboard, file
transfer, Universal Control.

**What does not:** screen mirror/cast, the continuity camera and the earbuds
hand-off. Those are GStreamer/GTK/PulseAudio/BlueZ and have no Windows
implementation, so they are compiled out rather than shipped broken.

**Two things to expect on first run.** The binary is unsigned, so SmartScreen
warns — *More info → Run anyway*. And on Windows 10 the window can open blank:
that is a missing **WebView2 runtime** (Windows 11 ships it). Install
Microsoft's Evergreen WebView2 Runtime and relaunch.

> ⚠ **Experimental, and less tested than Linux.** The BLE layer here is a
> separate implementation against a platform seam (WinRT rather than BlueZ),
> and it has had far less real-world use. Treat it as a preview and please
> report what breaks.

## 🔐 Security

All device-to-device traffic is **end-to-end encrypted**:
- Pairing: Noise XX for first pairing, Noise IK for trusted reconnect
- Runtime: ChaCha20-Poly1305
- Key storage: hardware-backed (Android Keystore, Linux Secret Service)

## 💬 Community

- **Telegram:** [t.me/vortexconnect](https://t.me/vortexconnect) — release
  announcements + discussion group (English and Uzbek welcome)
- **GitHub Issues** — bug reports (include phone model + ROM and Linux
  distro + desktop; BLE quirks are very device-specific)
- **GitHub Discussions** — questions and ideas

## 🤝 Contributing

Bug reports and issues are very welcome — especially device/ROM-specific BLE
quirks. PRs: see [CONTRIBUTING.md](CONTRIBUTING.md).

## 📜 License

**[GPLv3](LICENSE)** — forks stay open.

## 🌟 Design philosophy

- **Zero-config UX** — pairing once, everything just flows
- **Noise-based security** — the Signal/WireGuard school: few primitives, used correctly
- **Offline-first** — your data never needs anyone's server

---

**Built with ❤️ for Linux + Android users**
