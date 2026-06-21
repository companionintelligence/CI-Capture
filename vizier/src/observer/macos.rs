use std::process::Command;

use anyhow::Result;
use chrono::{Datelike, Local, NaiveDateTime, TimeZone};
use core_foundation::base::{CFType, TCFType};
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_graphics::display::CGDisplay;
use core_graphics::event::CGEvent;
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::window;
use serde_json::Value;
use sysinfo::System;

use crate::observation::{
    Bounds, DisplayInfo, GpuInfo, Point, RunningProcessInfo, SessionInfo, WakeObservation,
    WindowInfo,
};
use crate::observer::common::{BaselineObserver, BaselineWaker};
use crate::observer::{Observer, ObserverConfig, WakeConfig, Waker};

pub fn create_observer(config: ObserverConfig) -> Box<dyn Observer> {
    Box::new(MacObserver {
        baseline: BaselineObserver::new(config),
    })
}

pub fn create_waker(config: WakeConfig) -> Box<dyn Waker> {
    Box::new(MacWaker {
        baseline: BaselineWaker::new(config),
    })
}

struct MacObserver {
    baseline: BaselineObserver,
}

impl Observer for MacObserver {
    fn snapshot(&mut self) -> Result<crate::observation::Observation> {
        let mut observation = self.baseline.snapshot()?;

        let displays = collect_displays();
        if !displays.is_empty() {
            observation.displays = displays;
        }

        let windows = collect_windows(&observation.displays);
        if !windows.is_empty() {
            observation.focus = Some(windows[0].clone());
            observation.windows = windows;
        }

        if let Some(cursor) = cursor_position() {
            observation.cursor = cursor;
        }

        if let Some(idle_ms) = idle_ms() {
            observation.idle_ms = idle_ms;
        }

        Ok(observation)
    }
}

struct MacWaker {
    baseline: BaselineWaker,
}

impl Waker for MacWaker {
    fn wake(&self) -> Result<WakeObservation> {
        let mut wake = self.baseline.wake()?;

        wake.machine.os = "macOS".to_string();

        if let Some(version) = command_stdout("sw_vers", &["-productVersion"]) {
            wake.machine.os_version = version;
        }

        if let Some(kernel) = command_stdout("uname", &["-r"]) {
            wake.machine.kernel = format!("Darwin {kernel}");
        }

        if let Some(model) = command_stdout("sysctl", &["-n", "hw.model"]) {
            wake.machine.chassis = if model.starts_with("MacBook") {
                "Laptop".to_string()
            } else {
                "Desktop".to_string()
            };
        }

        let groups = user_groups();
        if !groups.is_empty() {
            wake.user.groups = groups;
        }

        if let Some(default_gateway) = default_gateway() {
            wake.network_identity.default_gateway = Some(default_gateway);
        }

        let dns = dns_servers();
        if !dns.is_empty() {
            wake.network_identity.dns_servers = dns;
        }

        let gpus = gpu_info();
        if !gpus.is_empty() {
            wake.resources.gpus = gpus;
        }

        if let Some(uptime_seconds) = uptime_seconds_from_boottime(wake.ts) {
            wake.datetime.uptime_seconds = uptime_seconds;
            wake.datetime.login_ts = wake.ts - uptime_seconds as f64;
        }

        let running_since_boot = running_since_boot(wake.ts as u64);
        if !running_since_boot.is_empty() {
            wake.recent_activity.running_since_boot = running_since_boot;
        }

        let sessions = other_sessions();
        if !sessions.is_empty() {
            wake.datetime.login_ts = sessions
                .iter()
                .map(|x| x.login_ts)
                .fold(wake.datetime.login_ts, f64::min);
            wake.other_sessions = sessions;
        }

        Ok(wake)
    }
}

fn collect_displays() -> Vec<DisplayInfo> {
    let main_display = CGDisplay::main().id;

    match CGDisplay::active_displays() {
        Ok(ids) => ids
            .into_iter()
            .map(|id| {
                let display = CGDisplay::new(id);
                let bounds = display.bounds();
                let (x, y, w, h) = (
                    bounds.origin.x.round() as i32,
                    bounds.origin.y.round() as i32,
                    bounds.size.width.round() as i32,
                    bounds.size.height.round() as i32,
                );

                let scale_factor = display
                    .display_mode()
                    .map(|mode| {
                        if mode.width() == 0 {
                            1.0
                        } else {
                            mode.pixel_width() as f64 / mode.width() as f64
                        }
                    })
                    .unwrap_or(1.0);

                DisplayInfo {
                    id: id as i32,
                    bounds: Bounds { x, y, w, h },
                    is_primary: id == main_display,
                    scale_factor,
                }
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn collect_windows(displays: &[DisplayInfo]) -> Vec<WindowInfo> {
    let options =
        window::kCGWindowListOptionOnScreenOnly | window::kCGWindowListExcludeDesktopElements;
    let windows = match window::create_window_list(options, window::kCGNullWindowID)
        .and_then(window::create_description_from_array)
    {
        Some(windows) => windows,
        None => return Vec::new(),
    };

    let key_number = unsafe { CFString::wrap_under_get_rule(window::kCGWindowNumber) };
    let key_layer = unsafe { CFString::wrap_under_get_rule(window::kCGWindowLayer) };
    let key_owner_pid = unsafe { CFString::wrap_under_get_rule(window::kCGWindowOwnerPID) };
    let key_owner_name = unsafe { CFString::wrap_under_get_rule(window::kCGWindowOwnerName) };
    let key_name = unsafe { CFString::wrap_under_get_rule(window::kCGWindowName) };
    let key_workspace = unsafe { CFString::wrap_under_get_rule(window::kCGWindowWorkspace) };

    let mut output = Vec::new();

    for entry in windows.iter() {
        let layer = dict_i64(&entry, &key_layer).unwrap_or(0);
        if layer != 0 {
            continue;
        }

        let id = dict_i64(&entry, &key_number).unwrap_or(0).to_string();
        let pid = dict_i64(&entry, &key_owner_pid).unwrap_or(0).max(0) as u32;
        let app = dict_string(&entry, &key_owner_name).unwrap_or_else(|| "Unknown".to_string());
        let title = dict_string(&entry, &key_name).unwrap_or_else(|| app.clone());
        let workspace = dict_i64(&entry, &key_workspace).unwrap_or(0) as i32;
        let bounds = Bounds {
            x: 0,
            y: 0,
            w: displays.first().map(|x| x.bounds.w).unwrap_or(0),
            h: displays.first().map(|x| x.bounds.h).unwrap_or(0),
        };
        let is_fullscreen = is_window_fullscreen(&bounds, displays);

        output.push(WindowInfo {
            id,
            title,
            app,
            pid,
            bounds,
            workspace,
            is_minimized: false,
            is_fullscreen,
        });
    }

    output
}

fn dict_string(dict: &CFDictionary<CFString, CFType>, key: &CFString) -> Option<String> {
    dict.find(key)
        .and_then(|value| value.downcast::<CFString>())
        .map(|value| value.to_string())
        .filter(|value| !value.is_empty())
}

fn dict_i64(dict: &CFDictionary<CFString, CFType>, key: &CFString) -> Option<i64> {
    dict.find(key)
        .and_then(|value| value.downcast::<CFNumber>())
        .and_then(|value| value.to_i64())
}

fn is_window_fullscreen(bounds: &Bounds, displays: &[DisplayInfo]) -> bool {
    displays.iter().any(|display| {
        let dw = display.bounds.w.max(1) as f64;
        let dh = display.bounds.h.max(1) as f64;

        let width_ratio = bounds.w.max(0) as f64 / dw;
        let height_ratio = bounds.h.max(0) as f64 / dh;
        width_ratio >= 0.95 && height_ratio >= 0.95
    })
}

fn cursor_position() -> Option<Point> {
    let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState).ok()?;
    let event = CGEvent::new(source).ok()?;
    let location = event.location();

    Some(Point {
        x: location.x.round() as i32,
        y: location.y.round() as i32,
    })
}

fn idle_ms() -> Option<u64> {
    let output = command_stdout("ioreg", &["-c", "IOHIDSystem"])?;
    let marker = "\"HIDIdleTime\" = ";
    let line = output.lines().find(|line| line.contains(marker))?;
    let raw = line.split_once(marker)?.1.trim();
    let nanos = raw.parse::<u64>().ok()?;
    Some(nanos / 1_000_000)
}

fn user_groups() -> Vec<String> {
    command_stdout("id", &["-Gn"])
        .map(|x| {
            x.split_whitespace()
                .map(|s| s.to_string())
                .collect::<Vec<String>>()
        })
        .unwrap_or_default()
}

fn default_gateway() -> Option<String> {
    let output = command_stdout("netstat", &["-nr"])?;
    output.lines().find_map(|line| {
        let line = line.trim();
        if !line.starts_with("default") {
            return None;
        }

        let cols: Vec<&str> = line.split_whitespace().collect();
        let gateway = *cols.get(1)?;
        if gateway.starts_with("link#") {
            return None;
        }

        Some(gateway.to_string())
    })
}

fn dns_servers() -> Vec<String> {
    let output = match command_stdout("scutil", &["--dns"]) {
        Some(output) => output,
        None => return Vec::new(),
    };

    let mut servers = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if let Some((_, server)) = line.split_once("nameserver[")
            && let Some((_, value)) = server.split_once(":")
        {
            servers.push(value.trim().to_string());
        }
    }

    servers.sort();
    servers.dedup();
    servers
}

fn gpu_info() -> Vec<GpuInfo> {
    let output = match command_stdout("system_profiler", &["SPDisplaysDataType", "-json"]) {
        Some(output) => output,
        None => return Vec::new(),
    };

    let root: Value = match serde_json::from_str(&output) {
        Ok(root) => root,
        Err(_) => return Vec::new(),
    };

    let gpus = root
        .get("SPDisplaysDataType")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();

    gpus.into_iter()
        .map(|gpu| {
            let name = gpu
                .get("sppci_model")
                .and_then(|value| value.as_str())
                .unwrap_or("unknown")
                .to_string();

            GpuInfo {
                name,
                vram_gb: None,
                driver: "metal".to_string(),
            }
        })
        .collect()
}

fn uptime_seconds_from_boottime(now_ts: f64) -> Option<u64> {
    let output = command_stdout("sysctl", &["-n", "kern.boottime"])?;
    let sec_fragment = output.split("sec = ").nth(1)?;
    let boot_sec = sec_fragment.split(',').next()?.trim().parse::<u64>().ok()?;
    let now = now_ts as u64;

    if boot_sec < 946_684_800 || boot_sec > now {
        return None;
    }

    Some(now.saturating_sub(boot_sec))
}

fn running_since_boot(now_ts: u64) -> Vec<RunningProcessInfo> {
    let mut system = System::new_all();
    system.refresh_all();

    let boot_time = System::boot_time();
    if boot_time == 0 {
        return Vec::new();
    }

    let mut processes: Vec<RunningProcessInfo> = system
        .processes()
        .values()
        .filter(|process| process.start_time() <= boot_time.saturating_add(120))
        .map(|process| RunningProcessInfo {
            pid: process.pid().as_u32(),
            app: process.name().to_string_lossy().to_string(),
            started_ago_s: now_ts.saturating_sub(process.start_time()),
        })
        .collect();

    processes.sort_by_key(|process| process.started_ago_s);
    processes.reverse();
    processes.truncate(20);
    processes
}

fn other_sessions() -> Vec<SessionInfo> {
    let output = match command_stdout("who", &[]) {
        Some(output) => output,
        None => return Vec::new(),
    };

    output
        .lines()
        .filter_map(parse_who_line)
        .collect::<Vec<SessionInfo>>()
}

fn parse_who_line(line: &str) -> Option<SessionInfo> {
    let cols: Vec<&str> = line.split_whitespace().collect();
    if cols.len() < 5 {
        return None;
    }

    let username = cols[0].to_string();
    let tty = cols[1].to_string();
    let month = cols[2];
    let day = cols[3];
    let time = cols[4];

    let mut year = Local::now().year();
    let parse_with_year = |year: i32| -> Option<NaiveDateTime> {
        NaiveDateTime::parse_from_str(&format!("{month} {day} {time} {year}"), "%b %e %H:%M %Y")
            .ok()
    };

    let mut naive = parse_with_year(year)?;
    let now = Local::now().naive_local();
    if naive > now {
        year -= 1;
        naive = parse_with_year(year)?;
    }

    let login_ts = Local
        .from_local_datetime(&naive)
        .earliest()
        .map(|dt| dt.timestamp() as f64)
        .unwrap_or(0.0);

    let from = line
        .split('(')
        .nth(1)
        .and_then(|x| x.split(')').next())
        .map(|x| x.to_string())
        .unwrap_or_else(|| "local".to_string());

    Some(SessionInfo {
        username,
        tty,
        from,
        login_ts,
    })
}

fn command_stdout(bin: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(bin).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }

    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim().to_string();

    if value.is_empty() { None } else { Some(value) }
}
