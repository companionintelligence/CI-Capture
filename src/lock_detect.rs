//! Whether this desktop session is locked right now — the gate that decides
//! whether the screenshot lane's next tick captures anything at all.
//!
//! Each platform answers this through its own private or semi-private
//! surface; there is no cross-platform crate for it. `is_locked()` is the
//! only symbol callers need — the per-OS mechanics live in their own `imp`
//! module below, mirroring the `permissions`/`app_names` shape used
//! elsewhere in this crate.

/// True when the desktop is locked (or, on macOS, when there is no console
/// session at all). `worker::is_capture_blocked` is the sole caller; a
/// locked screen should never reach the screenshot pipeline, so this errs
/// toward "locked" rather than "unlocked" whenever the underlying probe is
/// ambiguous — see each `imp` for exactly where that applies.
pub fn is_locked() -> bool {
    imp::is_locked()
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

    /// `pgrep -x ScreenSaverEngine` (the previous check) is false whenever the
    /// screen is locked via `loginwindow` without a screen saver ever having
    /// started — the ordinary result of Lock Screen / closing the lid /
    /// `pmset displaysleepnow`, not just an idle screen-saver timeout. Reading
    /// `CGSSessionScreenIsLocked` out of the session dictionary answers the
    /// question `loginwindow` itself is authoritative about, regardless of
    /// how the lock was triggered.
    pub(super) fn is_locked() -> bool {
        // SAFETY: `CGSessionCopyCurrentDictionary` follows CF's "create rule" —
        // a non-null return is a +1 reference this scope owns and must
        // release exactly once, which the `CFRelease` at the end of every
        // path below does. The returned dictionary's values (including the
        // boolean this function reads) follow the "get rule": borrowed for
        // the dictionary's lifetime, never separately retained or released.
        unsafe {
            let dict = CGSessionCopyCurrentDictionary();
            if dict.is_null() {
                // No session dictionary at all means no user is attached to
                // this console session (fast-user-switched away, or the
                // process is running headless over SSH with no logged-in
                // GUI user) — there is certainly no unlocked desktop to
                // screenshot, so this counts as locked rather than as "we
                // don't know".
                return true;
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
            locked
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    pub(super) fn is_locked() -> bool {
        false
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
}
