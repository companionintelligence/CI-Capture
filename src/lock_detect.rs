//! Whether this desktop session is locked right now — the gate that decides
//! whether the screenshot lane's next tick captures anything at all.
//!
//! Each platform answers this through its own private or semi-private
//! surface; there is no cross-platform crate for it. `is_locked()` is the
//! only symbol callers need — the per-OS mechanics live in their own `imp`
//! module below, mirroring the `permissions`/`app_names` shape used
//! elsewhere in this crate.
//!
//! Alongside lock state, this module also exposes `idle_ms()` — how long
//! since the last keyboard/mouse input, where the platform can answer that
//! at all. `worker::capture_once` uses it to pause the screenshot lane on a
//! desktop that is unlocked but has simply gone untouched for a full
//! capture interval, without treating "no answer" as "idle" (see each
//! `imp` for why that distinction matters per OS).

/// True when the desktop is locked (or, on macOS, when there is no console
/// session at all). `worker::is_capture_blocked` is the sole caller; a
/// locked screen should never reach the screenshot pipeline, so this errs
/// toward "locked" rather than "unlocked" whenever the underlying probe is
/// ambiguous — see each `imp` for exactly where that applies.
pub fn is_locked() -> bool {
    imp::is_locked()
}

/// Milliseconds since the last keyboard/mouse input, when the platform can
/// answer that question at all. `None` — not `Some(0)` — is how an `imp`
/// says "no signal here"; `worker::capture_once` must treat that as "we
/// don't know", never as "not idle" nor as "idle", so it never gates
/// capture on a missing probe. See each `imp` for which platforms return
/// `None` and why.
pub fn idle_ms() -> Option<u64> {
    imp::idle_ms()
}

#[cfg(target_os = "macos")]
mod imp {
    use core::ffi::c_void;

    use core_foundation::base::{CFRelease, TCFType};
    use core_foundation::dictionary::{CFDictionaryGetValueIfPresent, CFDictionaryRef};
    use core_foundation::number::CFBooleanGetValue;
    use core_foundation::string::CFString;

    // `CGSessionCopyCurrentDictionary` has never been a public header
    // declaration — it ships in `ApplicationServices` (re-exported from the
    // private `SkyLight`/`CoreGraphics` session-server surface) and is the
    // same call `pmset`, `caffeinate`, and most third-party lock-state tools
    // use, since Apple has never published a replacement. It is unavailable
    // to sandboxed App Store apps; CI-Capture ships Developer-ID signed
    // outside the Mac App Store (see docs/RELEASING.md), so that restriction
    // does not apply here.
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn CGSessionCopyCurrentDictionary() -> CFDictionaryRef;
    }

    /// What the session server answered, with "there was no session
    /// dictionary at all" kept distinct from "there was one and it says
    /// unlocked".
    ///
    /// `is_locked` collapses this to a bool and both of the non-`Unlocked`
    /// arms become `true`, which is the right call for the capture gate but
    /// throws away the one distinction a *test* needs. A `true` from
    /// `is_locked` alone cannot tell "the screen is genuinely locked" from
    /// "this probe is broken and returned nothing" — they are the same
    /// value. Keeping the three states separate here means the suite can
    /// assert the probe reached a real session dictionary without also
    /// asserting that whoever is running the tests has not locked their
    /// screen. See this module's tests.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum SessionLockState {
        /// No session dictionary: no user attached to this console session
        /// (fast-user-switched away, or running headless over SSH with no
        /// logged-in GUI user) — or the probe itself failed.
        NoSession,
        /// A real session dictionary that does not report a locked screen.
        Unlocked,
        /// A real session dictionary reporting `CGSSessionScreenIsLocked`.
        Locked,
    }

    impl SessionLockState {
        /// Only a dictionary that exists *and* reports an unlocked screen
        /// counts as unlocked. `NoSession` folds in with `Locked`
        /// deliberately: there is certainly no unlocked desktop to
        /// screenshot in that state, so the gate errs toward capturing
        /// nothing rather than treating "we don't know" as permission to
        /// capture.
        ///
        /// Split from `is_locked` below so the contract is a pure function
        /// over the three states — testable on any machine, in any lock
        /// state, without going near CoreFoundation.
        pub(super) fn is_locked(self) -> bool {
            !matches!(self, SessionLockState::Unlocked)
        }
    }

    /// `pgrep -x ScreenSaverEngine` (the previous check) is false whenever the
    /// screen is locked via `loginwindow` without a screen saver ever having
    /// started — the ordinary result of Lock Screen / closing the lid /
    /// `pmset displaysleepnow`, not just an idle screen-saver timeout. Reading
    /// `CGSSessionScreenIsLocked` out of the session dictionary answers the
    /// question `loginwindow` itself is authoritative about, regardless of
    /// how the lock was triggered.
    pub(super) fn session_lock_state() -> SessionLockState {
        // SAFETY: `CGSessionCopyCurrentDictionary` follows CF's "create rule" —
        // a non-null return is a +1 reference this scope owns and must
        // release exactly once, which the `CFRelease` at the end of every
        // path below does. The returned dictionary's values (including the
        // boolean this function reads) follow the "get rule": borrowed for
        // the dictionary's lifetime, never separately retained or released.
        unsafe {
            let dict = CGSessionCopyCurrentDictionary();
            if dict.is_null() {
                return SessionLockState::NoSession;
            }

            let key = CFString::new("CGSSessionScreenIsLocked");
            let key_ref = key.as_concrete_TypeRef().cast::<c_void>();
            let mut value: *const c_void = std::ptr::null();
            let locked = if CFDictionaryGetValueIfPresent(dict, key_ref, &mut value) != 0
                && !value.is_null()
            {
                // Present and true only while the screen is actually locked;
                // Apple's own documented convention for this key is that its
                // *absence* means unlocked, matched by the `else` branch.
                CFBooleanGetValue(value.cast())
            } else {
                false
            };

            CFRelease(dict.cast());
            if locked {
                SessionLockState::Locked
            } else {
                SessionLockState::Unlocked
            }
        }
    }

    /// The module-level probe: ask the session server, then collapse the
    /// answer through [`SessionLockState::is_locked`].
    pub(super) fn is_locked() -> bool {
        session_lock_state().is_locked()
    }

    /// `vizier::observer::macos::idle_ms` already shells out to
    /// `ioreg -c IOHIDSystem` and parses `HIDIdleTime` for the vizier
    /// observation snapshot; reusing it here means the capture worker's
    /// idle gate and vizier's own idle field can never disagree, and this
    /// crate never grows a second `ioreg` parser to keep in sync with the
    /// first.
    pub(super) fn idle_ms() -> Option<u64> {
        vizier::observer::macos::idle_ms()
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use core::mem::size_of;

    use windows_sys::Win32::Foundation::FALSE;
    use windows_sys::Win32::System::StationsAndDesktops::{
        CloseDesktop, DESKTOP_SWITCHDESKTOP, OpenInputDesktop,
    };
    use windows_sys::Win32::System::SystemInformation::GetTickCount;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

    /// A locked (or secure-desktop, e.g. UAC prompt / Ctrl+Alt+Del) session
    /// cannot open the *interactive* input desktop — `OpenInputDesktop` is
    /// the standard Windows technique for detecting this, used by e.g.
    /// Chromium's `IsWorkstationLocked` and countless idle-timer utilities,
    /// because there is no direct "is this session locked" query on
    /// Windows the way `loginwindow`'s session dictionary answers it on
    /// macOS. `dwFlags` is 0 (no special open flags), `fInherit` is `FALSE`
    /// (the returned handle should not be inherited by child processes),
    /// and `dwDesiredAccess` only needs `DESKTOP_SWITCHDESKTOP` — the
    /// narrowest access right that still requires the desktop to actually
    /// be switchable-to, which is exactly the property a locked session
    /// lacks.
    pub(super) fn is_locked() -> bool {
        // SAFETY: `OpenInputDesktop` takes only plain integer arguments and
        // returns either a valid `HDESK` or null on failure; there is no
        // aliasing or lifetime concern in the call itself. A non-null
        // handle is closed exactly once via `CloseDesktop` before
        // returning, matching the Win32 handle-ownership contract.
        unsafe {
            let hdesk = OpenInputDesktop(0, FALSE, DESKTOP_SWITCHDESKTOP);
            if hdesk.is_null() {
                // Could not open the interactive desktop: the session is on
                // the secure desktop (Lock Screen, UAC, Ctrl+Alt+Del), which
                // is the locked state this function exists to report.
                return true;
            }
            CloseDesktop(hdesk);
            false
        }
    }

    /// `GetLastInputInfo` reports the tick count (`GetTickCount`, ms since
    /// boot) at the last keyboard/mouse event system-wide; subtracting it
    /// from the current tick count gives idle duration without needing a
    /// window handle or message-loop hook. `GetTickCount` wraps to 0 every
    /// ~49.7 days of uptime, and a machine that has been on that long is
    /// exactly the kind of long-running capture host this crate targets, so
    /// the subtraction uses `wrapping_sub` to stay correct across that
    /// wrap instead of underflowing into a multi-billion-millisecond idle
    /// reading for one tick.
    pub(super) fn idle_ms() -> Option<u64> {
        let mut info = LASTINPUTINFO {
            cbSize: size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };

        // SAFETY: `info` is a valid, correctly-sized `LASTINPUTINFO` with
        // `cbSize` set before the call, which is the one precondition
        // `GetLastInputInfo` documents; a zero return means the call
        // failed and `info.dwTime` was left unwritten, so that path is
        // treated as "no signal" rather than read.
        let ok = unsafe { GetLastInputInfo(&mut info) };
        if ok == FALSE {
            return None;
        }

        let now = unsafe { GetTickCount() };
        Some(now.wrapping_sub(info.dwTime) as u64)
    }
}

#[cfg(target_os = "linux")]
mod imp {
    /// `org.freedesktop.ScreenSaver.GetActive` is the closest thing to a
    /// universal lock-state query on Linux — GNOME, KDE and every
    /// xscreensaver-compatible frontend register this interface on the
    /// session bus, even though none of them agree on much else. There is
    /// no kernel- or X11-level "is locked" signal to fall back to that
    /// would work across both X11 and Wayland compositors.
    ///
    /// Any failure to ask the question — no session bus (a headless CI
    /// runner, an SSH session with no D-Bus, a bare TTY), or a bus with no
    /// `org.freedesktop.ScreenSaver` service registered (a minimal window
    /// manager with no screen-locker running at all) — is treated as
    /// *unlocked*, deliberately, not as "unknown" or "locked". This is the
    /// opposite default from macOS's "missing session dictionary ⇒
    /// locked": on macOS that absence only happens on an actual
    /// fast-user-switch, a rare and genuinely lock-like state, while on
    /// Linux "no D-Bus session" is the ordinary shape of a desktop-less
    /// capture host that was never locked in the first place. Defaulting
    /// to "locked" there would silently and permanently stop capture on
    /// every such host.
    pub(super) fn is_locked() -> bool {
        query_screensaver_active().unwrap_or(false)
    }

    fn query_screensaver_active() -> zbus::Result<bool> {
        let connection = zbus::blocking::Connection::session()?;
        let proxy = zbus::blocking::Proxy::new(
            &connection,
            "org.freedesktop.ScreenSaver",
            "/org/freedesktop/ScreenSaver",
            "org.freedesktop.ScreenSaver",
        )?;
        proxy.call::<_, _, bool>("GetActive", &())
    }

    /// A deliberate, named scope cut, not a silent gap: idle duration needs
    /// either X11's `XScreenSaverQueryInfo` (which needs a real X11 display
    /// connection — meaningless on a Wayland-only session) or Wayland's
    /// `ext-idle-notify-v1` (which needs a bound Wayland connection this
    /// crate does not otherwise hold, unlike the D-Bus session `is_locked`
    /// above already needs regardless). Building and holding either
    /// connection just for idle sampling is a larger addition than this
    /// change, and is tracked separately — see the open CI-Engineering
    /// issue for Linux vizier/idle work. Returning `None` here, and having
    /// callers treat `None` as "no signal" rather than "not idle", is what
    /// keeps this cut safe rather than silently wrong.
    pub(super) fn idle_ms() -> Option<u64> {
        None
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
mod imp {
    pub(super) fn is_locked() -> bool {
        false
    }

    pub(super) fn idle_ms() -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one thing every OS can promise about this function: it returns
    /// without panicking. Real lock-state coverage lives in `imp`'s own
    /// per-OS tests where the platform can actually be driven into a locked
    /// state; CI has no display to lock.
    #[test]
    fn is_locked_does_not_panic() {
        let _ = is_locked();
    }

    /// Same promise for `idle_ms`: it never panics, on any OS, regardless
    /// of whether this particular host can actually answer the question.
    #[test]
    fn idle_ms_does_not_panic() {
        let _ = idle_ms();
    }

    /// The gate's contract, as a pure mapping over the three states the
    /// probe can report — no machine state involved, so this runs
    /// everywhere, including on CI and on a locked laptop.
    ///
    /// The load-bearing arm is `NoSession`. It has to read as locked: if
    /// "we could not ask" ever became "not locked", a fast-user-switched or
    /// headless Mac would start feeding the capture pipeline from a desktop
    /// nobody is looking at.
    #[test]
    #[cfg(target_os = "macos")]
    fn macos_only_a_real_unlocked_session_counts_as_unlocked() {
        use super::imp::SessionLockState;

        assert!(!SessionLockState::Unlocked.is_locked());
        assert!(SessionLockState::Locked.is_locked());
        assert!(
            SessionLockState::NoSession.is_locked(),
            "a missing session dictionary must err toward locked, never toward capturing"
        );
    }

    /// The live half: the CoreFoundation probe must actually reach a session
    /// dictionary on a real Mac. This is what catches the plumbing breaking
    /// — a bad framework link, a renamed symbol, a sandbox that refuses the
    /// call — all of which surface as `NoSession`.
    ///
    /// It deliberately asserts *which* of the three states this is not,
    /// rather than what `is_locked()` returns. An earlier version asserted
    /// `!is_locked()`, which failed whenever the developer's screen happened
    /// to be locked while the suite ran — a real flake on an unattended
    /// machine, and one that conflated "the probe is broken" with "the
    /// screen is locked", since `is_locked` returns `true` for both. Asking
    /// only "did we reach a session dictionary at all" keeps the regression
    /// coverage and holds in either lock state.
    ///
    /// Skipped wherever the *precondition* — this process is attached to a
    /// GUI console session — legitimately does not hold, because a test that
    /// fails on an unmet precondition is the flake this one is replacing.
    /// That is CI (a runner may have no console session) and SSH (a remote
    /// shell is outside the console session even when someone is logged in
    /// at the Mac, so the call correctly returns null). In both, `NoSession`
    /// is the right answer rather than a regression. See
    /// `is_locked_does_not_panic` for why the other platforms stop at "does
    /// not panic".
    #[test]
    #[cfg(target_os = "macos")]
    fn macos_probe_reaches_a_real_session_dictionary() {
        use super::imp::{SessionLockState, session_lock_state};

        let no_gui_session = std::env::var_os("CI").is_some()
            || std::env::var_os("SSH_CONNECTION").is_some()
            || std::env::var_os("SSH_TTY").is_some();
        if no_gui_session {
            return;
        }
        assert_ne!(
            session_lock_state(),
            SessionLockState::NoSession,
            "the CoreFoundation probe did not reach a session dictionary on a machine \
             that has one — the framework link or the symbol is broken, which \
             `is_locked()` alone would have reported indistinguishably from a locked screen"
        );
    }

    /// macOS idle time is read from the same live `ioreg` counter `is_locked`
    /// exercises above; it must come back with a value at all (the desktop
    /// this suite runs on has a console session), even if that value is 0.
    #[test]
    #[cfg(target_os = "macos")]
    fn macos_idle_ms_returns_a_value_on_a_live_interactive_session() {
        if std::env::var_os("CI").is_some() {
            return;
        }
        assert!(idle_ms().is_some());
    }

    /// Windows idle time must never fabricate a signal it does not have:
    /// `GetLastInputInfo` degrades to `None` on failure rather than reading
    /// an unwritten `dwTime`, which `idle_ms_does_not_panic` above already
    /// exercises end to end. This documents the contract for callers who
    /// cannot run this file's Windows branch on a Mac: a `None` here must
    /// never be treated as "not idle".
    #[test]
    #[cfg(target_os = "windows")]
    fn windows_idle_ms_never_panics_even_without_a_console_session() {
        let _ = idle_ms();
    }

    /// The Linux probe must degrade to `false`/`None` rather than panicking
    /// or blocking forever when there is no session bus at all — the
    /// ordinary shape of the sandboxed/headless runner this test suite
    /// itself usually runs on. `is_locked_does_not_panic` and
    /// `idle_ms_does_not_panic` already cover "doesn't panic"; this pins
    /// down the specific "no bus ⇒ unlocked" default `is_locked` documents,
    /// on whatever bus (or lack of one) this runner actually has.
    #[test]
    #[cfg(target_os = "linux")]
    fn linux_is_locked_degrades_to_false_without_a_screensaver_service() {
        // A real desktop session with a screensaver service registered
        // would make this assertion meaningless (it could legitimately be
        // locked), so this only pins the no-bus/no-service default down on
        // a headless runner, matching every other gated test in this file.
        if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
            assert!(!is_locked());
        }
    }

    /// Linux idle detection is a deliberate, documented scope cut — this
    /// pins that contract down as a test, not just a comment, so a future
    /// change that starts returning `Some` here is a visible, reviewable
    /// decision rather than an accidental regression of the "no signal"
    /// guarantee callers depend on.
    #[test]
    #[cfg(target_os = "linux")]
    fn linux_idle_ms_is_always_none() {
        assert_eq!(idle_ms(), None);
    }
}
