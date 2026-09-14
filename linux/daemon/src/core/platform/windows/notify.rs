//! Desktop notifications over WinRT toasts — the Windows half of [`Notifier`].
//!
//! # Why this is a thread and not a function
//!
//! Showing a toast is easy; getting the click back is the whole problem. The
//! click arrives as an `Activated` event on the `ToastNotification` object, so
//! *something must keep that object alive* for as long as the toast is on
//! screen — drop it and the button goes nowhere. Toast objects are also not
//! agile, so they cannot be parked in a `Mutex` and touched from whichever tokio
//! worker happens to be free.
//!
//! So one thread owns the notifier, the live toasts, and their event
//! subscriptions; the async trait methods are a channel in front of it. This is
//! the same shape as the advertisement watcher in [`super::ble`], and the same
//! shape `SECRET_RT` uses for libsecret on Linux.
//!
//! # What still has to be true outside this file
//!
//! An unpackaged desktop app has no AppUserModelID until something registers
//! one, and `CreateToastNotifierWithId` with an unregistered AUMID shows
//! *nothing at all* — no error, no toast. The installer must create a
//! Start-menu shortcut carrying [`AUMID`]. That is a packaging job, not a code
//! one, and it is the single most likely reason a Windows build shows no
//! notifications while every call here returns `Ok`.
//!
//! Activation while Vortex is **not running** additionally needs a registered
//! COM activator (`INotificationActivationCallback`). Vortex is a long-running
//! daemon, so in-process `Activated` events cover the cases that matter (file
//! consent, the call banner); a cold-start click is simply lost, which is no
//! worse than the notification never having been shown.
//!
//! None of this has been run — see the note in [`super::ble`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Mutex, OnceLock};

use windows::core::{IInspectable, Interface, HSTRING};
use windows::Data::Xml::Dom::XmlDocument;
use windows::Foundation::TypedEventHandler;
use windows::UI::Notifications::{
    ToastActivatedEventArgs, ToastDismissalReason, ToastDismissedEventArgs, ToastNotification,
    ToastNotificationManager,
};

use super::ensure_winrt;
use crate::core::platform::toast_xml::toast_xml;
use crate::core::platform::{BoxFuture, Notifier};

/// The AppUserModelID toasts are shown under. Must match the AUMID on the
/// Start-menu shortcut [`register_aumid_shortcut`] creates, or nothing appears.
pub const AUMID: &str = "com.vortex.desktop";

/// Give this AUMID a Start-menu shortcut, so toasts are actually displayed.
///
/// Windows will not show a toast for an AppUserModelID it cannot resolve to an
/// installed app, and it does not consider that an error: `CreateToastNotifier`
/// succeeds, `Show` succeeds, and nothing appears. That is exactly what the
/// first working Windows run did — a file-consent banner was "shown", no one
/// saw it, and the 45-second timeout declined the transfer.
///
/// The registration IS a shortcut in the user's Start menu whose
/// `System.AppUserModel.ID` property holds the AUMID. Nothing about it needs an
/// installer or admin rights, which is why this is done here rather than left
/// to packaging: a standalone .exe can register itself on first run and toasts
/// start working. An MSI would write the same shortcut.
///
/// Idempotent by existence check. Cheap, and rewriting the shortcut on every
/// launch would reset a name or icon the user customised.
///
/// # Unverified
///
/// Type-checked only, like everything else in this module. The failure mode if
/// it is wrong is the one we already have — silent, invisible toasts — so it
/// logs what it did either way.
pub fn register_aumid_shortcut() {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{Interface, GUID, PCWSTR, PWSTR};
    use windows::Win32::Foundation::{E_OUTOFMEMORY, PROPERTYKEY};
    use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PROPVARIANT};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoTaskMemAlloc, IPersistFile, CLSCTX_INPROC_SERVER,
    };
    use windows::Win32::System::Variant::VT_LPWSTR;
    use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink, FOLDERID_Programs};

    ensure_winrt();

    let Some(dir) = super::known_folder(&FOLDERID_Programs) else {
        tracing::warn!("toasts: no Start-menu Programs folder; AUMID unregistered");
        return;
    };
    let link_path = dir.join("Vortex.lnk");
    if link_path.exists() {
        tracing::debug!(path = %link_path.display(), "toasts: AUMID shortcut already present");
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        tracing::warn!("toasts: cannot resolve own path; AUMID unregistered");
        return;
    };

    // `System.AppUserModel.ID`. Spelled out rather than imported because the
    // PKEY_* names are C macros that do not survive into the bindings.
    const PKEY_APPUSERMODEL_ID: PROPERTYKEY = PROPERTYKEY {
        fmtid: GUID::from_u128(0x9F4C2855_9F79_4B39_A8D0_E1D42DE1D5F3),
        pid: 5,
    };

    // Null-terminated UTF-16 in locals that outlive every call below: a PCWSTR
    // is a borrowed pointer, so a temporary would hand the shell freed memory.
    let exe_w: Vec<u16> = exe.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let link_w: Vec<u16> =
        link_path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let aumid_w: Vec<u16> = AUMID.encode_utf16().chain(std::iter::once(0)).collect();

    let result = (|| -> windows::core::Result<()> {
        // SAFETY: a straight-line COM sequence — create the shell-link object,
        // point it at this exe, stamp the AUMID into its property store, save.
        // Each fallible step is checked before the next runs, and every pointer
        // handed over borrows a local declared above.
        unsafe {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            link.SetPath(PCWSTR(exe_w.as_ptr()))?;
            // The Start-menu label, and the name Windows attributes the toast to.
            link.SetDescription(&HSTRING::from("Vortex"))?;

            // PROPVARIANT has no safe constructor in this crate version, so it
            // is assembled by hand — but the string it points at MUST come from
            // the COM allocator.
            //
            // A PROPVARIANT is an OWNING value by COM convention: whoever holds
            // one may call `PropVariantClear`, and for a `VT_LPWSTR` that means
            // `CoTaskMemFree(pwszVal)`. Pointing it at a Rust `Vec<u16>`, as
            // this did, hands a Rust heap block to the COM allocator — and then
            // the `Vec` frees the same block again on the way out. That is
            // heap corruption, and it is what killed the first Windows run
            // (`STATUS_HEAP_CORRUPTION`, 0xc0000374, faulting in ntdll)
            // milliseconds after this function first created the shortcut. It
            // could only ever happen once per machine, because the existence
            // check above skips all of this on every later run — which is
            // exactly the "crashed once, fine afterwards" shape it had.
            //
            // So: allocate with `CoTaskMemAlloc`, and clear the variant
            // ourselves when we are done. One allocator throughout, one free.
            let bytes = std::mem::size_of_val(&aumid_w[..]);
            let com_str = CoTaskMemAlloc(bytes) as *mut u16;
            if com_str.is_null() {
                return Err(windows::core::Error::from(E_OUTOFMEMORY));
            }
            std::ptr::copy_nonoverlapping(aumid_w.as_ptr(), com_str, aumid_w.len());

            let mut pv = PROPVARIANT::default();
            {
                let inner = &mut *pv.Anonymous.Anonymous;
                inner.vt = VT_LPWSTR;
                inner.Anonymous.pwszVal = PWSTR(com_str);
            }
            let store: IPropertyStore = link.cast()?;
            // Clear on BOTH paths: `SetValue` copies the value, so the buffer is
            // ours to release whether it succeeded or not. `PropVariantClear`
            // zeroes the variant too, so the later drop cannot double-free.
            let set = store.SetValue(&PKEY_APPUSERMODEL_ID, &pv);
            let _ = PropVariantClear(&mut pv);
            set?;
            store.Commit()?;

            let file: IPersistFile = link.cast()?;
            file.Save(PCWSTR(link_w.as_ptr()), true)?;
            Ok(())
        }
    })();
    match result {
        Ok(()) => tracing::info!(
            path = %link_path.display(),
            aumid = AUMID,
            "toasts: registered AUMID shortcut (notifications should now appear)",
        ),
        Err(e) => tracing::warn!(
            "toasts: AUMID shortcut failed ({}); toasts stay invisible",
            err_str(&e)
        ),
    }
}

/// WinRT/COM error to string, for the log lines above.
fn err_str(e: &windows::core::Error) -> String {
    format!("{} ({})", e.message(), e.code().0)
}

/// Where clicks land — EVERY registrant, not the first.
///
/// The freedesktop side broadcasts: each consumer subscribes to the bus and
/// sees every `ActionInvoked`, filtering by key prefix (`fc:` file consent,
/// `call:` the call banner, `act:` a mirrored action). Three consumers do
/// exactly that. A single-slot sink would hand the stream to whichever happened
/// to register first and leave the others permanently dead — clicks on a
/// consent prompt doing nothing, with nothing in the log to say why. So this
/// fans out, which is the same contract on both platforms.
static ACTION_SINKS: OnceLock<Mutex<Vec<tokio::sync::mpsc::UnboundedSender<(u32, String)>>>> =
    OnceLock::new();
/// Same, for dismissals.
static CLOSURE_SINKS: OnceLock<Mutex<Vec<tokio::sync::mpsc::UnboundedSender<(u32, u32)>>>> =
    OnceLock::new();

fn action_sinks() -> &'static Mutex<Vec<tokio::sync::mpsc::UnboundedSender<(u32, String)>>> {
    ACTION_SINKS.get_or_init(|| Mutex::new(Vec::new()))
}

fn closure_sinks() -> &'static Mutex<Vec<tokio::sync::mpsc::UnboundedSender<(u32, u32)>>> {
    CLOSURE_SINKS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Deliver to every registered consumer, dropping the ones whose receiver has
/// gone. Pruning here rather than on registration keeps a long-lived process
/// from accumulating dead senders across reconnects.
fn fan_out_action(id: u32, key: String) {
    if let Ok(mut sinks) = action_sinks().lock() {
        sinks.retain(|tx| tx.send((id, key.clone())).is_ok());
    }
}

fn fan_out_closure(id: u32, reason: u32) {
    if let Ok(mut sinks) = closure_sinks().lock() {
        sinks.retain(|tx| tx.send((id, reason)).is_ok());
    }
}

/// Notification ids we hand out. Starts at 1 so 0 keeps its freedesktop
/// meaning of "no id / do not replace".
static NEXT_ID: AtomicU32 = AtomicU32::new(1);

enum Cmd {
    Show {
        summary: String,
        body: String,
        actions: Vec<(String, String)>,
        replaces: u32,
        urgent: bool,
        reply: tokio::sync::oneshot::Sender<Result<u32, String>>,
    },
    Close(u32),
}

fn commands() -> &'static Mutex<Option<Sender<Cmd>>> {
    static TX: OnceLock<Mutex<Option<Sender<Cmd>>>> = OnceLock::new();
    TX.get_or_init(|| Mutex::new(None))
}

/// Hand a command to the toast thread, starting it on first use.
fn send(cmd: Cmd) -> Result<(), String> {
    let mut guard = commands().lock().map_err(|_| "toast channel poisoned")?;
    if guard.is_none() {
        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        std::thread::Builder::new()
            .name("vortex-toasts".into())
            .spawn(move || toast_thread(rx))
            .map_err(|e| format!("spawn toast thread: {e}"))?;
        *guard = Some(tx);
    }
    guard
        .as_ref()
        .expect("just set")
        .send(cmd)
        .map_err(|_| "toast thread is gone".to_string())
}

/// Owns the notifier and every live toast, start to finish.
fn toast_thread(rx: Receiver<Cmd>) {
    ensure_winrt();
    // Before the first notifier, not at app startup: this is the one place that
    // needs the AUMID to resolve, it runs exactly once, and a build that never
    // shows a notification never pays for it.
    register_aumid_shortcut();
    let notifier = match ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID)) {
        Ok(n) => n,
        Err(e) => {
            // Nothing to fall back to: without a notifier every Show would
            // fail anyway, and the AUMID note at the top of this file is the
            // likely cause.
            tracing::error!("toast notifier unavailable (AUMID registered?): {e}");
            return;
        }
    };
    // id → (toast, tag). The toast is held so its Activated/Dismissed handlers
    // stay alive; the tag is how Windows identifies a toast for replace/remove,
    // since it has no notion of our u32 ids.
    let mut live: HashMap<u32, (ToastNotification, String)> = HashMap::new();

    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Show {
                summary,
                body,
                actions,
                replaces,
                urgent,
                reply,
            } => {
                // Replacing means reusing the previous toast's tag: Windows
                // then updates in place instead of stacking a second banner.
                // That is what the progress notifications rely on.
                let (id, tag) = match live.get(&replaces) {
                    Some((_, tag)) => (replaces, tag.clone()),
                    None => {
                        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
                        (id, format!("vortex-{id}"))
                    }
                };
                let result = show_toast(&notifier, id, &tag, &summary, &body, &actions, urgent);
                match result {
                    Ok(toast) => {
                        live.insert(id, (toast, tag));
                        let _ = reply.send(Ok(id));
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }
            Cmd::Close(id) => {
                if let Some((_, tag)) = live.remove(&id) {
                    // Remove by tag, not by object: a toast already dismissed
                    // by the user is gone from history and this is a no-op.
                    if let Ok(history) = ToastNotificationManager::History() {
                        let _ = history.RemoveGroupedTagWithId(
                            &HSTRING::from(tag),
                            &HSTRING::from("vortex"),
                            &HSTRING::from(AUMID),
                        );
                    }
                }
            }
        }
    }
}

fn show_toast(
    notifier: &windows::UI::Notifications::ToastNotifier,
    id: u32,
    tag: &str,
    summary: &str,
    body: &str,
    actions: &[(String, String)],
    urgent: bool,
) -> Result<ToastNotification, String> {
    let xml = XmlDocument::new().map_err(|e| format!("XmlDocument: {e}"))?;
    xml.LoadXml(&HSTRING::from(toast_xml(summary, body, actions, urgent)))
        .map_err(|e| format!("toast xml: {e}"))?;
    let toast = ToastNotification::CreateToastNotification(&xml)
        .map_err(|e| format!("CreateToastNotification: {e}"))?;
    toast
        .SetTag(&HSTRING::from(tag))
        .map_err(|e| format!("SetTag: {e}"))?;
    toast
        .SetGroup(&HSTRING::from("vortex"))
        .map_err(|e| format!("SetGroup: {e}"))?;

    // A click carries the `arguments` of the button pressed — the same
    // `fc:` / `call:` / `act:` keys the Linux path emits, so the routing above
    // this trait needs no Windows-specific branch. A click on the toast BODY
    // has empty arguments and is dropped: the consumers key off a prefix, and
    // "the user clicked somewhere" is not one of their actions.
    let activated = TypedEventHandler::<ToastNotification, IInspectable>::new(
        move |_toast, args| {
            if let Some(args) = args.as_ref() {
                if let Ok(a) = args.cast::<ToastActivatedEventArgs>() {
                    if let Ok(key) = a.Arguments() {
                        let key = key.to_string_lossy();
                        if !key.is_empty() {
                            fan_out_action(id, key);
                        }
                    }
                }
            }
            Ok(())
        },
    );
    toast
        .Activated(&activated)
        .map_err(|e| format!("Activated: {e}"))?;

    // Dismissal reasons are mapped onto the freedesktop `NotificationClosed`
    // codes the existing consumers already interpret (1 expired, 2 dismissed by
    // the user, 3 withdrawn by us, 4 unknown), so a dismissal mirrored back to
    // the phone means the same thing on both platforms.
    let dismissed = TypedEventHandler::<ToastNotification, ToastDismissedEventArgs>::new(
        move |_toast, args| {
            let reason = args
                .as_ref()
                .and_then(|a| a.Reason().ok())
                .map(|r| match r {
                    ToastDismissalReason::TimedOut => 1u32,
                    ToastDismissalReason::UserCanceled => 2,
                    ToastDismissalReason::ApplicationHidden => 3,
                    _ => 4,
                })
                .unwrap_or(4);
            fan_out_closure(id, reason);
            Ok(())
        },
    );
    toast
        .Dismissed(&dismissed)
        .map_err(|e| format!("Dismissed: {e}"))?;

    notifier.Show(&toast).map_err(|e| format!("Show: {e}"))?;
    Ok(toast)
}

pub struct WindowsNotifier;

impl Notifier for WindowsNotifier {
    fn show(
        &self,
        summary: &str,
        body: &str,
        _app_id: &str,
        actions: &[(String, String)],
        replaces: u32,
        urgent: bool,
    ) -> BoxFuture<Result<u32, String>> {
        // `app_id` is a freedesktop concept (the desktop-entry name, which is
        // how the shell finds the icon). Windows takes the identity from the
        // AUMID and the icon from that shortcut, so there is nothing to pass it
        // to — see AUMID above.
        // Everything owned up front (the `&str`s do not outlive this call),
        // but the queueing itself happens on await — building a future must
        // not show a notification.
        let summary = summary.to_string();
        let body = body.to_string();
        let actions = actions.to_vec();
        Box::pin(async move {
            let (reply, rx) = tokio::sync::oneshot::channel();
            send(Cmd::Show {
                summary,
                body,
                actions,
                replaces,
                urgent,
                reply,
            })?;
            rx.await
                .map_err(|_| "toast thread dropped the request".to_string())?
        })
    }

    fn close(&self, id: u32) -> BoxFuture<Result<(), String>> {
        Box::pin(async move { send(Cmd::Close(id)) })
    }

    fn actions(&self, tx: tokio::sync::mpsc::UnboundedSender<(u32, String)>) {
        // Additive: every consumer gets every click, as on the bus.
        if let Ok(mut sinks) = action_sinks().lock() {
            sinks.push(tx);
        }
    }

    fn closures(&self, tx: tokio::sync::mpsc::UnboundedSender<(u32, u32)>) {
        if let Ok(mut sinks) = closure_sinks().lock() {
            sinks.push(tx);
        }
    }
}
