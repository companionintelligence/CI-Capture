//! OS capture-permission status + request aids for the onboarding wizard.
//!
//! The applet's capture lanes silently produce nothing without the right OS
//! grants — on macOS a missing **Screen Recording** grant yields wallpaper-only
//! frames. This module surfaces the live grant state and provides one-click
//! aids: trigger the system prompt where the OS allows it, and deep-link to the
//! exact System Settings pane otherwise.
//!
//! **Screen Recording is the only grant that gates anything.** Accessibility is
//! reported for diagnostics but deliberately does not affect [`Permissions::all_ok`]
//! or [`Permissions::needs_attention`]: nothing in the applet or in `vizier`
//! reads a single Accessibility API. Window titles and focus come from
//! `CGWindowListCopyWindowInfo` (`vizier/src/observer/macos.rs`), which TCC gates
//! behind Screen Recording, and idle detection uses `CGEventSource`, which needs
//! no grant at all. Treating a missing AX grant as a problem previously pinned
//! the tray to "⚠︎ Permissions needed" forever on a perfectly working install.
//!
//! macOS gates screen/AX capture behind TCC. GNOME on Wayland gates screenshots
//! behind the desktop portal, which remembers one answer per app, so the
//! screenshot lane follows the same path there (see `gnome` below). Everywhere
//! else these report [`PermissionStatus::NotRequired`] so the wizard can skip
//! the step.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionStatus {
    /// Confirmed granted.
    #[allow(dead_code)]
    Granted,
    /// Confirmed missing — capture will silently fail until the user grants it.
    Denied,
    /// Nothing to grant: either this OS does not gate the capability behind a
    /// permission at all, or the resource the permission would protect does
    /// not exist on this machine.
    ///
    /// Reached on macOS too, which is what the three consumers of this enum
    /// depend on: `full_disk_access_status` returns it when there is no
    /// `~/Library/Messages/chat.db`, i.e. Messages has never run here. That
    /// user must not be sent to the Full Disk Access pane — granting it would
    /// change nothing — so it is deliberately NOT a flavour of `Denied`.
    NotRequired,
}

impl PermissionStatus {
    pub fn is_ok(self) -> bool {
        matches!(self, Self::Granted | Self::NotRequired)
    }

    /// A short label + traffic-light glyph for the UI.
    ///
    /// Localized, so this owns its text rather than returning `&'static str`:
    /// the label depends on the active locale, which is only known at runtime.
    pub fn badge(self) -> String {
        match self {
            Self::Granted => rust_i18n::t!("status.permission.granted"),
            Self::Denied => rust_i18n::t!("status.permission.denied"),
            Self::NotRequired => rust_i18n::t!("status.permission.not_required"),
        }
        .to_string()
    }
}

impl fmt::Display for PermissionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.badge())
    }
}

/// The permissions the applet's capture lanes care about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permissions {
    /// Screen Recording — required for the screenshot lane.
    pub screen_recording: PermissionStatus,
    /// Accessibility — reported for diagnostics only. Nothing reads AX data;
    /// see the module docs before wiring this into any gate.
    pub accessibility: PermissionStatus,
    /// Full Disk Access — required for the `imessage` lane to read
    /// `~/Library/Messages/chat.db`. Deliberately excluded from
    /// [`Self::all_ok`]/[`Self::needs_attention`], same reasoning as
    /// Accessibility above: those two gate the *original* (screenshot/vizier)
    /// lanes specifically, and folding a lane-specific grant into them would
    /// make an install with a perfectly working screenshot lane report a
    /// permissions problem because iMessage import is off. Read this field
    /// directly wherever the imessage lane's own readiness is what's being
    /// reported (see `crate::imessage`).
    pub full_disk_access: PermissionStatus,
}

impl Permissions {
    /// True when nothing is blocking capture.
    #[allow(dead_code)] // Part of the type's API; exercised by the tests below.
    pub fn all_ok(self) -> bool {
        self.screen_recording.is_ok()
    }

    /// True when a grant capture actually depends on is missing.
    pub fn needs_attention(self) -> bool {
        self.screen_recording == PermissionStatus::Denied
    }
}

/// Read the current permission state (non-prompting).
pub fn status() -> Permissions {
    Permissions {
        screen_recording: screen_recording_status(),
        accessibility: accessibility_status(),
        full_disk_access: full_disk_access_status(),
    }
}

// ── macOS ───────────────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
mod imp {
    use super::PermissionStatus;

    // Framework functions. CoreGraphics gates Screen Recording; the
    // Preflight/Request pair is the modern (10.15+) API — Preflight reads the
    // grant without prompting, Request triggers the system prompt once.
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGPreflightScreenCaptureAccess() -> bool;
        fn CGRequestScreenCaptureAccess() -> bool;
    }
    // ApplicationServices exposes the Accessibility (AX) trust check.
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> bool;
    }

    pub fn screen_recording_status() -> PermissionStatus {
        // SAFETY: parameterless Apple C ABI call returning a C bool.
        if unsafe { CGPreflightScreenCaptureAccess() } {
            PermissionStatus::Granted
        } else {
            PermissionStatus::Denied
        }
    }

    pub fn accessibility_status() -> PermissionStatus {
        // SAFETY: parameterless Apple C ABI call returning a Boolean.
        if unsafe { AXIsProcessTrusted() } {
            PermissionStatus::Granted
        } else {
            PermissionStatus::Denied
        }
    }

    /// Trigger the OS Screen Recording prompt (first call) and return the
    /// resulting grant state. Subsequent calls just report current state.
    pub fn request_screen_recording() -> PermissionStatus {
        // SAFETY: parameterless Apple C ABI call; may present a system dialog.
        if unsafe { CGRequestScreenCaptureAccess() } {
            PermissionStatus::Granted
        } else {
            PermissionStatus::Denied
        }
    }

    // System Settings deep links (stable `x-apple.systempreferences:` scheme).
    pub const SCREEN_RECORDING_PANE: &str =
        "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";
    pub const FULL_DISK_ACCESS_PANE: &str =
        "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles";

    /// Full Disk Access has no TCC preflight API (unlike Screen Recording's
    /// `CGPreflightScreenCaptureAccess`) — Apple only exposes it as a
    /// consequence of actually trying to read a protected path, never as a
    /// query. `stat`/`ls` on a TCC-protected file succeeds either way (they
    /// don't touch file *contents*), so the only honest check opens the file
    /// and reads a byte. `~/Library/Messages/chat.db` is the actual resource
    /// the imessage lane needs, so checking that file directly — rather than
    /// a generic canary like `TCC.db` — means this reports exactly what the
    /// lane can and can't do, including the "no Messages history on this Mac
    /// at all" case (no file → `NotRequired`, not `Denied`, since there is
    /// nothing to be denied access to).
    pub fn full_disk_access_status() -> super::PermissionStatus {
        let Some(home) = std::env::var_os("HOME") else {
            return super::PermissionStatus::Denied;
        };
        protected_file_status(&std::path::Path::new(&home).join("Library/Messages/chat.db"))
    }

    /// The open-and-read-a-byte probe behind [`full_disk_access_status`],
    /// for any other TCC-protected file a lane needs (the `knowledge` lane's
    /// `knowledgeC.db`). Each lane probes the file it will actually read:
    /// a Mac that never ran Messages has no `chat.db`, so the imessage probe
    /// reports `NotRequired` there even though `knowledgeC.db` exists and is
    /// still gated — one canary cannot speak for another.
    pub fn protected_file_status(path: &std::path::Path) -> super::PermissionStatus {
        use std::io::Read;

        if !path.exists() {
            return super::PermissionStatus::NotRequired;
        }

        match std::fs::File::open(path).and_then(|mut f| {
            let mut buf = [0u8; 16];
            f.read_exact(&mut buf)
        }) {
            Ok(()) => super::PermissionStatus::Granted,
            Err(_) => super::PermissionStatus::Denied,
        }
    }
}

// ── non-macOS: only GNOME on Wayland gates anything ─────────────────────────

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::PermissionStatus;

    pub fn screen_recording_status() -> PermissionStatus {
        #[cfg(target_os = "linux")]
        if super::gnome::is_gated() {
            return super::gnome::status();
        }
        PermissionStatus::NotRequired
    }
    pub fn accessibility_status() -> PermissionStatus {
        PermissionStatus::NotRequired
    }
    pub fn request_screen_recording() -> PermissionStatus {
        #[cfg(target_os = "linux")]
        if super::gnome::is_gated() {
            super::gnome::ask();
            return super::gnome::status();
        }
        PermissionStatus::NotRequired
    }
    pub const SCREEN_RECORDING_PANE: &str = "";
    pub const FULL_DISK_ACCESS_PANE: &str = "";
    pub fn full_disk_access_status() -> PermissionStatus {
        PermissionStatus::NotRequired
    }
    pub fn protected_file_status(_path: &std::path::Path) -> PermissionStatus {
        PermissionStatus::NotRequired
    }
}

// ── GNOME on Wayland ────────────────────────────────────────────────────────

/// GNOME on Wayland takes a screenshot for an ordinary app only through
/// `org.freedesktop.portal.Screenshot`, which asks the person once and stores the
/// answer per app in the portal's permission store. Three things about that
/// decide how the lane has to behave (`js/ui/accessDialog.js` in gnome-shell):
///
/// - An app with an identity may only raise the dialog while it is the focused
///   app. A tray app in the background is refused on the spot, so the portal
///   fails and the capture helper falls through to libwayshot's refusal, tick
///   after tick, with nothing on screen. The ask has to come from a window the
///   person is looking at, which is the setup assistant's Permission step.
/// - Only one dialog shows at a time. It outlives the helper that raised it, so
///   killing a helper doesn't take the question away.
/// - An answer applies from then on with no further prompt. Deny is remembered
///   the same way.
#[cfg(target_os = "linux")]
mod gnome {
    use std::collections::HashMap;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use super::PermissionStatus;

    /// The store is read on the setup window's and the tray's threads, so a hung
    /// service has to read as "can't tell" instead of freezing them.
    const STORE_TIMEOUT: Duration = Duration::from_secs(2);

    /// True while the last helper in this process handed back a frame: the only
    /// proof that needs no knowledge of how the portal identified us. A failed
    /// helper clears it, so a grant taken back in Settings is noticed.
    pub static PROVEN: AtomicBool = AtomicBool::new(false);
    /// True while the helper raised by [`ask`] is still waiting on the dialog. The
    /// `screenshots` crate stops waiting after 60 seconds, the dialog doesn't.
    pub static ASKING: AtomicBool = AtomicBool::new(false);

    pub fn is_gated() -> bool {
        let var = |name| std::env::var(name).unwrap_or_default();
        gated_by(
            &var("XDG_SESSION_TYPE"),
            &var("WAYLAND_DISPLAY"),
            &var("XDG_CURRENT_DESKTOP"),
        )
    }

    pub fn gated_by(session_type: &str, wayland_display: &str, current_desktop: &str) -> bool {
        (session_type == "wayland" || !wayland_display.is_empty())
            && current_desktop
                .split(':')
                .any(|desktop| desktop.eq_ignore_ascii_case("gnome"))
    }

    /// Reads the stored answer. No prompt, so this is safe to poll. A store that
    /// can't be read says nothing either way and must not stop capture.
    pub fn status() -> PermissionStatus {
        if PROVEN.load(Ordering::Relaxed) {
            return PermissionStatus::Granted;
        }
        match stored_answers() {
            Ok(answers) => status_from(&answers, &own_app_id()),
            Err(err) => {
                log::debug!("portal permission store unavailable: {err}");
                PermissionStatus::NotRequired
            }
        }
    }

    pub fn status_from(answers: &HashMap<String, Vec<String>>, app_id: &str) -> PermissionStatus {
        match answers.get(app_id).and_then(|answer| answer.first()) {
            Some(answer) if answer == "yes" => PermissionStatus::Granted,
            _ => PermissionStatus::Denied,
        }
    }

    /// One connection per process, kept once it works. The store is read once a
    /// second while the setup window is open.
    fn store_connection() -> zbus::Result<&'static zbus::blocking::Connection> {
        static CONNECTION: OnceLock<zbus::blocking::Connection> = OnceLock::new();
        if let Some(connection) = CONNECTION.get() {
            return Ok(connection);
        }
        let connection = zbus::blocking::connection::Builder::session()?
            .method_timeout(STORE_TIMEOUT)
            .build()?;
        Ok(CONNECTION.get_or_init(|| connection))
    }

    fn stored_answers() -> zbus::Result<HashMap<String, Vec<String>>> {
        let proxy = zbus::blocking::Proxy::new(
            store_connection()?,
            "org.freedesktop.impl.portal.PermissionStore",
            "/org/freedesktop/impl/portal/PermissionStore",
            "org.freedesktop.impl.portal.PermissionStore",
        )?;
        let reply = proxy.call_method("Lookup", &("screenshot", "screenshot"));
        match reply {
            Ok(message) => {
                let (answers, _data): (HashMap<String, Vec<String>>, zbus::zvariant::OwnedValue) =
                    message.body().deserialize()?;
                Ok(answers)
            }
            // The table doesn't exist until the first answer on this machine.
            Err(zbus::Error::MethodError(name, _, _))
                if name.as_str() == "org.freedesktop.portal.Error.NotFound" =>
            {
                Ok(HashMap::new())
            }
            Err(err) => Err(err),
        }
    }

    /// The id the portal files this process under, read the way it does: from the
    /// systemd unit the desktop started us in. Empty means an unsandboxed process
    /// it can't name, such as one started from a terminal.
    fn own_app_id() -> String {
        std::fs::read_to_string("/proc/self/cgroup")
            .ok()
            .and_then(|cgroup| {
                let unit = cgroup.lines().last()?.rsplit('/').next()?.to_string();
                app_id_from_unit(&unit)
            })
            .unwrap_or_default()
    }

    /// `app[-<launcher>]-<id>-<random>.scope` and `app[-<launcher>]-<id>[@<random>].service`
    /// (systemd.io/DESKTOP_ENVIRONMENTS), or `snap.<name>.<app>-<random>.scope`.
    pub fn app_id_from_unit(unit: &str) -> Option<String> {
        if let Some(rest) = unit.strip_prefix("snap.") {
            return Some(format!("snap.{}", rest.split('.').next()?));
        }
        let rest = unit.strip_prefix("app-")?;
        let body = if let Some(scope) = rest.strip_suffix(".scope") {
            scope.rsplit_once('-')?.0
        } else {
            rest.strip_suffix(".service")?.split('@').next()?
        };
        let id = match body.split_once('-') {
            Some((launcher, id))
                if !id.is_empty() && launcher.chars().all(char::is_alphanumeric) =>
            {
                id
            }
            _ => body,
        };
        Some(unescape_unit(id))
    }

    /// systemd writes `-` inside a name as `\x2d`.
    fn unescape_unit(name: &str) -> String {
        let mut out = Vec::with_capacity(name.len());
        let bytes = name.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let escaped = (bytes[i] == b'\\' && bytes.get(i + 1) == Some(&b'x'))
                .then(|| name.get(i + 2..i + 4))
                .flatten()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok());
            match escaped {
                Some(byte) => {
                    out.push(byte);
                    i += 4;
                }
                None => {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// Raises the dialog, by taking one frame through the portal, on a thread so
    /// the setup window keeps drawing. Call it from a click: see the module note
    /// on focus.
    pub fn ask() {
        if ASKING.swap(true, Ordering::Relaxed) {
            return;
        }
        std::thread::spawn(|| {
            let result =
                crate::capture::capture_isolated_within(crate::capture::PERMISSION_PROMPT_TIMEOUT);
            match result {
                Ok(_) => PROVEN.store(true, Ordering::Relaxed),
                Err(failure) => log::info!("permission check took no frame: {failure}"),
            }
            ASKING.store(false, Ordering::Relaxed);
        });
    }
}

/// The screenshot lane should wait for a person on this desktop instead of
/// running a helper every tick: GNOME on Wayland with no stored yes yet.
pub fn screen_capture_awaits_permission() -> bool {
    #[cfg(target_os = "linux")]
    {
        gnome::is_gated() && gnome::status() == PermissionStatus::Denied
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// How the last helper went. A frame means capture works here whatever the store
/// says, and that proof lasts only until a helper fails.
pub fn note_screen_capture_result(worked: bool) {
    #[cfg(target_os = "linux")]
    gnome::PROVEN.store(worked, std::sync::atomic::Ordering::Relaxed);
    #[cfg(not(target_os = "linux"))]
    let _ = worked;
}

/// Whether to show screen capture as allowed. On GNOME a store that can't be read
/// isn't an answer, so the person keeps the Allow button, whose frame settles it.
pub fn screen_capture_confirmed(status: PermissionStatus) -> bool {
    if uses_desktop_portal() {
        status == PermissionStatus::Granted
    } else {
        status.is_ok()
    }
}

/// The setup window's ask is still waiting on the desktop's dialog.
pub fn permission_prompt_open() -> bool {
    #[cfg(target_os = "linux")]
    {
        gnome::ASKING.load(std::sync::atomic::Ordering::Relaxed)
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// True where the permission text has to talk about GNOME instead of macOS.
pub fn uses_desktop_portal() -> bool {
    #[cfg(target_os = "linux")]
    {
        gnome::is_gated()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

pub fn screen_recording_status() -> PermissionStatus {
    imp::screen_recording_status()
}
pub fn accessibility_status() -> PermissionStatus {
    imp::accessibility_status()
}

/// Ask the OS to prompt for Screen Recording (macOS) or screenshots (GNOME on
/// Wayland, where the answer arrives later); returns the status as of now.
/// No-op returning `NotRequired` elsewhere.
pub fn request_screen_recording() -> PermissionStatus {
    imp::request_screen_recording()
}

/// The System Settings pane URL for Screen Recording (empty off macOS).
pub fn screen_recording_settings_url() -> &'static str {
    imp::SCREEN_RECORDING_PANE
}

pub fn full_disk_access_status() -> PermissionStatus {
    imp::full_disk_access_status()
}

/// Whether this process can read `path`, a TCC-protected file (Full Disk
/// Access on macOS). Three states, same as [`full_disk_access_status`]:
/// `NotRequired` when the file does not exist (nothing to be denied access
/// to), `Granted` when a byte can be read, `Denied` otherwise. Always
/// `NotRequired` off macOS.
pub fn protected_file_status(path: &std::path::Path) -> PermissionStatus {
    imp::protected_file_status(path)
}

/// The System Settings pane URL for Full Disk Access (empty off macOS).
///
/// No `request_full_disk_access` exists to pair with it — unlike Screen
/// Recording there is no system prompt to trigger, only this deep link plus
/// the user manually adding/enabling Companion Capture in the list and (per
/// Apple's own behavior for this specific pane) relaunching the app.
pub fn full_disk_access_settings_url() -> &'static str {
    imp::FULL_DISK_ACCESS_PANE
}

/// Whether this OS gates capture behind permissions at all (drives whether the
/// onboarding wizard shows the permissions step).
pub fn permissions_relevant() -> bool {
    cfg!(target_os = "macos") || uses_desktop_portal()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_is_readable_without_panicking() {
        // The real point: calling into the framework FFI (macOS) or the stubs
        // (elsewhere) is safe and returns a coherent struct.
        let p = status();
        assert_eq!(
            p.screen_recording.is_ok(),
            p.screen_recording != PermissionStatus::Denied
        );
        // all_ok and needs_attention are never both true.
        assert!(!(p.all_ok() && p.needs_attention()));
    }

    /// Regression guard: a declined Accessibility grant must not make a working
    /// install report a problem, because no code path consumes AX data.
    #[test]
    fn accessibility_does_not_gate_capture_readiness() {
        let p = Permissions {
            screen_recording: PermissionStatus::Granted,
            accessibility: PermissionStatus::Denied,
            full_disk_access: PermissionStatus::Denied,
        };
        assert!(p.all_ok());
        assert!(!p.needs_attention());
    }

    #[test]
    fn missing_screen_recording_is_still_a_problem() {
        let p = Permissions {
            screen_recording: PermissionStatus::Denied,
            accessibility: PermissionStatus::Granted,
            full_disk_access: PermissionStatus::Granted,
        };
        assert!(!p.all_ok());
        assert!(p.needs_attention());
    }

    /// Same reasoning as the Accessibility test above: a missing FDA grant
    /// must not make an otherwise-working install (screenshot lane fine)
    /// report a permissions problem — imessage is a separate, off-by-default
    /// lane with its own readiness check.
    #[test]
    fn full_disk_access_does_not_gate_capture_readiness() {
        let p = Permissions {
            screen_recording: PermissionStatus::Granted,
            accessibility: PermissionStatus::Granted,
            full_disk_access: PermissionStatus::Denied,
        };
        assert!(p.all_ok());
        assert!(!p.needs_attention());
    }

    #[test]
    fn ok_and_badges() {
        assert!(PermissionStatus::Granted.is_ok());
        assert!(PermissionStatus::NotRequired.is_ok());
        assert!(!PermissionStatus::Denied.is_ok());
        assert!(PermissionStatus::Granted.badge().contains("Granted"));
        assert!(PermissionStatus::Denied.badge().contains("Not granted"));
    }

    /// The three-state probe behind both the imessage and knowledge FDA
    /// gates. A missing file is `NotRequired` on every OS — a Mac with no
    /// store at all must not be sent to the Full Disk Access pane, since
    /// there is nothing to be denied access to.
    #[test]
    fn protected_file_status_missing_file_is_not_required_everywhere() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.db");
        assert_eq!(
            protected_file_status(&missing),
            PermissionStatus::NotRequired
        );
    }

    /// A file this process can open and read a byte from is `Granted` —
    /// the only positive signal the probe has, since TCC offers no query API
    /// for Full Disk Access.
    #[cfg(target_os = "macos")]
    #[test]
    fn protected_file_status_readable_file_is_granted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("readable.db");
        std::fs::write(&path, [0u8; 32]).unwrap();
        assert_eq!(protected_file_status(&path), PermissionStatus::Granted);
    }

    /// Deliberate boundary, not an accident: the probe `read_exact`s 16
    /// bytes, so a 0-byte file the process CAN open is still reported as
    /// unreadable (`Denied`). A real SQLite store is never 0 bytes (the
    /// header alone is 100), so this pins the documented choice rather than
    /// a case a user will hit — and it guards against the probe quietly
    /// degrading to a bare `File::open`, which TCC does not always refuse.
    #[cfg(target_os = "macos")]
    #[test]
    fn protected_file_status_empty_file_is_denied() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.db");
        std::fs::write(&path, b"").unwrap();
        assert_eq!(protected_file_status(&path), PermissionStatus::Denied);
    }

    /// Off macOS there is no TCC, so the probe is a stub: even a perfectly
    /// readable file is `NotRequired`, never `Granted`, and the imessage
    /// wrapper agrees.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn protected_file_status_is_stubbed_off_macos() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("readable.db");
        std::fs::write(&path, [0u8; 32]).unwrap();
        assert_eq!(protected_file_status(&path), PermissionStatus::NotRequired);
        assert_eq!(full_disk_access_status(), PermissionStatus::NotRequired);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn non_macos_requires_nothing() {
        // A developer's own GNOME desktop is the one place this is gated.
        if uses_desktop_portal() {
            return;
        }
        assert!(!permissions_relevant());
        let p = status();
        assert_eq!(p.screen_recording, PermissionStatus::NotRequired);
        assert_eq!(p.accessibility, PermissionStatus::NotRequired);
        assert!(p.all_ok() && !p.needs_attention());
        assert!(screen_recording_settings_url().is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_gates_capture_and_has_settings_links() {
        assert!(permissions_relevant());
        assert!(screen_recording_settings_url().starts_with("x-apple.systempreferences:"));
        assert!(full_disk_access_settings_url().starts_with("x-apple.systempreferences:"));
        // Status resolves to a definite Granted/Denied (never NotRequired) on macOS.
        assert_ne!(accessibility_status(), PermissionStatus::NotRequired);
        assert_ne!(screen_recording_status(), PermissionStatus::NotRequired);
        // full_disk_access_status CAN be NotRequired here (no chat.db on this
        // Mac at all), unlike the other two — just check it doesn't panic and
        // returns something.
        let _ = full_disk_access_status();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn only_gnome_on_wayland_goes_through_the_portal() {
        use super::gnome::gated_by;
        assert!(gated_by("wayland", "wayland-0", "ubuntu:GNOME"));
        assert!(gated_by("wayland", "", "GNOME"));
        assert!(gated_by("", "wayland-0", "gnome"));
        assert!(!gated_by("x11", "", "ubuntu:GNOME"));
        assert!(!gated_by("wayland", "wayland-0", "KDE"));
        assert!(!gated_by("wayland", "wayland-0", "Hyprland"));
        assert!(!gated_by("", "", ""));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_app_id_is_read_from_the_unit_the_way_the_portal_reads_it() {
        use super::gnome::app_id_from_unit;
        let id = |unit| app_id_from_unit(unit);
        assert_eq!(
            id(r"app-gnome-companion\x2dcapture-4242.scope").as_deref(),
            Some("companion-capture")
        );
        assert_eq!(
            id(r"app-gnome-org.gnome.Evolution\x2dalarm\x2dnotify-2437.scope").as_deref(),
            Some("org.gnome.Evolution-alarm-notify")
        );
        assert_eq!(
            id("app-companion\\x2dcapture-4242.scope").as_deref(),
            Some("companion-capture")
        );
        assert_eq!(
            id("app-gnome-companion\\x2dcapture@abc123.service").as_deref(),
            Some("companion-capture")
        );
        // A login start has to come out as the desktop file's name, or GNOME
        // refuses the dialog and the stored answer doesn't apply.
        assert_eq!(
            id(r"app-companion\x2dcapture@autostart.service").as_deref(),
            Some(crate::config::DESKTOP_ID)
        );
        assert_eq!(
            id(r"app-Companion\x20Capture@autostart.service").as_deref(),
            Some("Companion Capture")
        );
        assert_eq!(
            id("snap.code.code-efd21498-e69f-48cd-b36b-f20c955893ff.scope").as_deref(),
            Some("snap.code")
        );
        // A terminal's scope isn't an app: the portal files that under "".
        assert_eq!(id("vte-spawn-58b0e4a1.scope"), None);
        assert_eq!(id("session-2.scope"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn only_a_stored_yes_for_this_app_counts_as_granted() {
        use super::gnome::status_from;
        use std::collections::HashMap;
        let store = HashMap::from([
            ("companion-capture".to_string(), vec!["yes".to_string()]),
            ("other".to_string(), vec!["no".to_string()]),
        ]);
        assert_eq!(
            status_from(&store, "companion-capture"),
            PermissionStatus::Granted
        );
        assert_eq!(status_from(&store, "other"), PermissionStatus::Denied);
        // Never answered. Someone else's yes doesn't carry over.
        assert_eq!(status_from(&store, ""), PermissionStatus::Denied);
        assert_eq!(status_from(&HashMap::new(), "x"), PermissionStatus::Denied);
    }
}
