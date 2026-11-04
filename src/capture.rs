use std::fs;
use std::io::{Read, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use image::{ColorType, ImageEncoder, RgbaImage, codecs::png::PngEncoder};
use screenshots::Screen;

#[derive(Clone, Debug)]
pub struct CapturedImage {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>, // RGBA
}

impl CapturedImage {
    pub fn to_png_bytes(&self) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut bytes);
        let encoder = PngEncoder::new(&mut cursor);
        encoder
            .write_image(&self.data, self.width, self.height, ColorType::Rgba8.into())
            .context("encoding png")?;
        Ok(bytes)
    }

    pub fn to_image(&self) -> Result<RgbaImage> {
        RgbaImage::from_raw(self.width, self.height, self.data.clone())
            .context("converting screenshot buffer")
    }
}

/// Capture the user's primary display.
///
/// Settings promises "Your main display", so this must pick the monitor the OS
/// calls primary rather than whichever one the enumeration happens to list
/// first. The two are not the same thing: `EnumDisplayMonitors` on Windows
/// and the xrandr/RandR output list on X11 walk monitors in adapter/connector
/// order, so a laptop docked to an external screen — or a desktop whose
/// secondary is plugged into the first port — routinely reports a *secondary*
/// display first. macOS's `CGGetActiveDisplayList` does put the main display
/// first, which is why the old `.next()` looked right on the one platform it
/// was written against. `is_primary` is the OS's own flag
/// (`MONITORINFOF_PRIMARY`, the RandR monitor `primary` flag,
/// `CGDisplayIsMain`) on every backend, so honouring it is correct
/// everywhere; the first-screen fallback covers a headless or virtual session
/// where nothing claims to be primary.
///
/// No unit test: `Screen::all()` needs a live display server, which none of
/// the three CI runners has in the test job. The selection rule is a one-line
/// `find` with a fallback and is covered by review.
pub fn capture_primary() -> Result<CapturedImage> {
    let screens = Screen::all().context("enumerating displays")?;
    let screen = screens
        .iter()
        .find(|s| s.display_info.is_primary)
        .or_else(|| screens.first())
        .context("no displays detected")?;
    let image = panic::catch_unwind(AssertUnwindSafe(|| screen.capture()))
        .map_err(|_| anyhow!("screen capture panicked"))?
        .context("capturing display")?;
    let width = image.width();
    let height = image.height();
    let data = image.into_raw();
    Ok(CapturedImage {
        width,
        height,
        data,
    })
}

pub fn diff_percent(prev: &CapturedImage, current: &CapturedImage) -> f32 {
    if prev.width != current.width || prev.height != current.height {
        return 100.0;
    }

    let total_pixels = (prev.width * prev.height) as usize;
    if total_pixels == 0 {
        return 0.0;
    }

    let mut changed_pixels = 0usize;
    for (px_prev, px_curr) in prev.data.chunks_exact(4).zip(current.data.chunks_exact(4)) {
        if px_prev != px_curr {
            changed_pixels += 1;
        }
    }

    (changed_pixels as f32 / total_pixels as f32) * 100.0
}

pub fn save_to_path(image: &CapturedImage, path: &Path) -> Result<PathBuf> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating screenshot directory {}", parent.display()))?;
    }
    let rgba = image.to_image()?;
    rgba.save(path)
        .with_context(|| format!("saving screenshot to {}", path.display()))?;
    Ok(path.to_path_buf())
}

/// Hidden argument that makes this binary a one-shot capture helper.
pub const CAPTURE_HELPER_FLAG: &str = "--capture-helper";

/// How long the parent waits for one helper before killing it.
const HELPER_TIMEOUT: Duration = Duration::from_secs(15);

/// The limit on a helper when a person just asked for the desktop's permission
/// dialog. The `screenshots` crate stops waiting on the portal after 60 seconds,
/// so this only has to outlast that. The dialog stays up and its answer is stored
/// whenever it comes.
#[cfg(target_os = "linux")]
pub const PERMISSION_PROMPT_TIMEOUT: Duration = Duration::from_secs(120);

/// Starts the stderr line a helper writes when it panics.
const HELPER_PANIC_PREFIX: &str = "capture-helper panic: ";

/// Why a helper produced no frame.
#[derive(Debug, PartialEq, Eq)]
pub enum HelperFailure {
    /// Nothing usable came back: the helper exited, hung, or sent a frame that
    /// does not add up. Logged once per streak and not reported to Sentry.
    Gap(String),
    /// The helper panicked for a reason other than the compositor's refusal.
    /// Logged once per streak and reported to Sentry, as the in-process panic
    /// was before the helper existed.
    Panic(String),
}

impl std::fmt::Display for HelperFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HelperFailure::Gap(message) | HelperFailure::Panic(message) => f.write_str(message),
        }
    }
}

/// Body of the `--capture-helper` process: take one frame, write it to stdout,
/// and return the exit code. It sets up no logging, Sentry, or UI, so the
/// parent is the only place a failure is logged or reported.
pub fn run_capture_helper() -> i32 {
    panic::set_hook(Box::new(|info| {
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|message| message.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".to_string());
        let location = info
            .location()
            .map(|at| format!("{}:{}", at.file(), at.line()))
            .unwrap_or_default();
        eprintln!("{HELPER_PANIC_PREFIX}{payload} at {location}");
    }));
    match capture_primary() {
        Ok(image) => match write_helper_frame(&image, &mut std::io::stdout().lock()) {
            Ok(()) => 0,
            Err(err) => {
                eprintln!("capture-helper could not write the frame: {err}");
                1
            }
        },
        Err(err) => {
            eprintln!("capture-helper error: {err:#}");
            1
        }
    }
}

/// Writes a frame as the helper's stdout: width and height as little-endian
/// `u32`, then the RGBA pixels.
fn write_helper_frame(image: &CapturedImage, out: &mut impl Write) -> std::io::Result<()> {
    out.write_all(&image.width.to_le_bytes())?;
    out.write_all(&image.height.to_le_bytes())?;
    out.write_all(&image.data)?;
    out.flush()
}

/// Reads a frame written by `write_helper_frame`. A short header, an empty
/// frame, or a pixel count that does not match the size is an error, never a
/// partial image.
pub fn decode_helper_frame(mut bytes: Vec<u8>) -> Result<CapturedImage, String> {
    if bytes.len() < 8 {
        return Err(format!("frame header truncated ({} bytes)", bytes.len()));
    }
    let width = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let height = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    if width == 0 || height == 0 {
        return Err(format!("frame has no pixels ({width}x{height})"));
    }
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| format!("frame size overflows ({width}x{height})"))?;
    let got = bytes.len() - 8;
    if got != expected {
        return Err(format!(
            "frame size mismatch: got {got} pixel bytes, expected {expected}"
        ));
    }
    bytes.drain(..8);
    Ok(CapturedImage {
        width,
        height,
        data: bytes,
    })
}

/// Sorts a helper that produced no frame into a gap or a reported panic.
/// `status` is the exit status as text; `stderr` is what the helper wrote
/// before it stopped.
pub fn classify_helper_failure(status: &str, stderr: &str) -> HelperFailure {
    let Some(panic) = stderr
        .lines()
        .find_map(|line| line.strip_prefix(HELPER_PANIC_PREFIX))
    else {
        let detail = stderr.lines().next().unwrap_or("").trim();
        return HelperFailure::Gap(if detail.is_empty() {
            format!("capture helper stopped ({status})")
        } else {
            format!("capture helper stopped ({status}): {detail}")
        });
    };
    // libwayshot panics with NotPresent when the compositor refuses a frame. That
    // is the expected gap. The same panic from anywhere else is a bug.
    if panic.contains("NotPresent") && panic.contains("libwayshot") {
        HelperFailure::Gap(format!("the compositor refused the frame: {panic}"))
    } else {
        HelperFailure::Panic(panic.to_string())
    }
}

/// Captures the primary display in a child process. libwayshot calls
/// `process::exit` when the compositor refuses a frame, which `catch_unwind`
/// cannot stop, so that call now only ends the helper. A helper that hangs is
/// killed after `HELPER_TIMEOUT`.
pub fn capture_isolated() -> Result<CapturedImage, HelperFailure> {
    capture_isolated_within(HELPER_TIMEOUT)
}

/// [`capture_isolated`] with the time limit chosen by the caller.
pub fn capture_isolated_within(timeout: Duration) -> Result<CapturedImage, HelperFailure> {
    let exe = std::env::current_exe()
        .map_err(|err| HelperFailure::Gap(format!("finding the capture helper: {err}")))?;
    let mut child = Command::new(exe)
        .arg(CAPTURE_HELPER_FLAG)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| HelperFailure::Gap(format!("starting the capture helper: {err}")))?;
    let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(HelperFailure::Gap(
            "capture helper pipes are missing".to_string(),
        ));
    };
    // Both pipes are read while the helper runs. A frame is bigger than a pipe
    // buffer, so reading only after exit would leave the helper blocked on write.
    let frame_reader = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let stderr_reader = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf
    });
    let status = wait_for_helper(&mut child, timeout);
    let frame = frame_reader.join().unwrap_or_default();
    let stderr = String::from_utf8_lossy(&stderr_reader.join().unwrap_or_default()).into_owned();
    let status = status.map_err(HelperFailure::Gap)?;
    if !status.success() {
        return Err(classify_helper_failure(&status.to_string(), &stderr));
    }
    decode_helper_frame(frame).map_err(HelperFailure::Gap)
}

/// Waits for the helper to exit. Past `timeout` it is killed and reaped.
fn wait_for_helper(child: &mut Child, timeout: Duration) -> Result<ExitStatus, String> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "capture helper timed out after {}s",
                    timeout.as_secs()
                ));
            }
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("waiting for the capture helper: {err}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
        let mut bytes = width.to_le_bytes().to_vec();
        bytes.extend_from_slice(&height.to_le_bytes());
        bytes.extend_from_slice(pixels);
        bytes
    }

    #[test]
    fn a_written_frame_decodes_to_the_same_image() {
        let image = CapturedImage {
            width: 2,
            height: 1,
            data: vec![1, 2, 3, 4, 5, 6, 7, 8],
        };
        let mut wire = Vec::new();
        write_helper_frame(&image, &mut wire).unwrap();
        let decoded = decode_helper_frame(wire).unwrap();
        assert_eq!((decoded.width, decoded.height), (2, 1));
        assert_eq!(decoded.data, image.data);
    }

    #[test]
    fn a_truncated_frame_is_an_error_not_a_partial_image() {
        let err = decode_helper_frame(frame(2, 1, &[1, 2, 3])).unwrap_err();
        assert!(err.contains("size mismatch"), "{err}");
    }

    #[test]
    fn a_frame_with_extra_bytes_is_rejected() {
        let err = decode_helper_frame(frame(1, 1, &[1, 2, 3, 4, 5])).unwrap_err();
        assert!(err.contains("size mismatch"), "{err}");
    }

    #[test]
    fn a_short_header_is_rejected() {
        let err = decode_helper_frame(vec![1, 0, 0]).unwrap_err();
        assert!(err.contains("header truncated"), "{err}");
    }

    #[test]
    fn an_empty_frame_is_rejected() {
        let err = decode_helper_frame(frame(0, 5, &[])).unwrap_err();
        assert!(err.contains("no pixels"), "{err}");
    }

    #[test]
    fn dimensions_that_overflow_are_rejected_before_any_allocation() {
        let err = decode_helper_frame(frame(u32::MAX, u32::MAX, &[])).unwrap_err();
        assert!(err.contains("overflows"), "{err}");
    }

    #[test]
    fn a_compositor_refusal_is_a_gap() {
        let stderr = "capture-helper panic: called `Result::unwrap()` on an `Err` value: NotPresent at /home/u/.cargo/registry/src/index/libwayshot-0.3.0/src/lib.rs:88:14\n";
        assert!(matches!(
            classify_helper_failure("exit status: 101", stderr),
            HelperFailure::Gap(_)
        ));
    }

    #[test]
    fn a_not_present_panic_from_elsewhere_is_still_reported() {
        let stderr = "capture-helper panic: NotPresent at src/capture.rs:12:5\n";
        assert_eq!(
            classify_helper_failure("exit status: 101", stderr),
            HelperFailure::Panic("NotPresent at src/capture.rs:12:5".to_string())
        );
    }

    #[test]
    fn any_other_panic_is_reported() {
        let stderr = "capture-helper panic: index out of bounds at src/capture.rs:10:5\n";
        assert_eq!(
            classify_helper_failure("exit status: 101", stderr),
            HelperFailure::Panic("index out of bounds at src/capture.rs:10:5".to_string())
        );
    }

    #[test]
    fn an_exit_with_a_helper_error_is_a_gap_that_keeps_the_error() {
        assert_eq!(
            classify_helper_failure(
                "exit status: 1",
                "capture-helper error: no displays detected\n"
            ),
            HelperFailure::Gap(
                "capture helper stopped (exit status: 1): capture-helper error: no displays detected"
                    .to_string()
            )
        );
    }

    #[test]
    fn a_silent_exit_is_a_gap() {
        assert_eq!(
            classify_helper_failure("exit status: 3", ""),
            HelperFailure::Gap("capture helper stopped (exit status: 3)".to_string())
        );
    }
}
