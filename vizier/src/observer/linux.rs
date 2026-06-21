use std::env;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;

use anyhow::Result;
use chrono::{Datelike, Local, NaiveDateTime, TimeZone};
use serde_json::Value;
use sysinfo::System;

use crate::observation::{
    Bounds, DisplayInfo, GpuInfo, RunningProcessInfo, SessionInfo, TerminalCtx, WakeObservation,
    WindowInfo,
};
use crate::observer::common::{BaselineObserver, BaselineWaker};
use crate::observer::{Observer, ObserverConfig, WakeConfig, Waker};

pub fn create_observer(config: ObserverConfig) -> Box<dyn Observer> {
    Box::new(LinuxObserver {
        baseline: BaselineObserver::new(config),
    })
}

pub fn create_waker(config: WakeConfig) -> Box<dyn Waker> {
    Box::new(LinuxWaker {
        baseline: BaselineWaker::new(config),
    })
}

struct LinuxObserver {
    baseline: BaselineObserver,
}

impl Observer for LinuxObserver {
    fn snapshot(&mut self) -> Result<crate::observation::Observation> {
        let mut observation = self.baseline.snapshot()?;

        let Some(socket_path) = hyprland_socket_path() else {
            return Ok(observation);
        };

        if let Some(monitors) = hyprland_monitors(&socket_path)
            && !monitors.is_empty()
        {
            observation.displays = monitors;
        }

        if let Some(windows) = hyprland_clients(&socket_path)
            && !windows.is_empty()
        {
            observation.windows = windows;
        }

        if let Some(focus) = hyprland_active_window(&socket_path) {
            observation.focus = Some(focus.clone());

            if is_terminal_app(&focus.app)
                && let Some(cwd) = linux_process_cwd(focus.pid)
            {
                observation.terminal_ctx = Some(TerminalCtx {
                    cwd,
                    shell: "unknown".to_string(),
                });
            }
        }

        Ok(observation)
    }
}

struct LinuxWaker {
    baseline: BaselineWaker,
}

impl Waker for LinuxWaker {
    fn wake(&self) -> Result<WakeObservation> {
        let mut wake = self.baseline.wake()?;

        wake.machine.os = "Linux".to_string();

        let os_info = os_release();
        if let Some(version) = os_info
            .get("VERSION_ID")
            .cloned()
            .or_else(|| os_info.get("PRETTY_NAME").cloned())
        {
            wake.machine.os_version = version;
        }

        if let Some(kernel) = command_stdout("uname", &["-r"]) {
            wake.machine.kernel = kernel;
        }

        wake.machine.is_container = wake.machine.is_container || detect_container();

        if let Some(chassis) = chassis_from_dmi() {
            wake.machine.chassis = chassis;
        }

        let groups = user_groups();
        if !groups.is_empty() {
            wake.user.groups = groups;
        }

        if let Some(default_gateway) = default_gateway() {
            wake.network_identity.default_gateway = Some(default_gateway);
        }

        let gpus = gpu_info();
        if !gpus.is_empty() {
            wake.resources.gpus = gpus;
        }

        if let Some(uptime_seconds) = linux_uptime_seconds() {
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

fn hyprland_socket_path() -> Option<PathBuf> {
    let signature = env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
    let runtime = env::var("XDG_RUNTIME_DIR").ok()?;

    let path = PathBuf::from(runtime)
        .join("hypr")
        .join(signature)
        .join(".socket.sock");

    if path.exists() { Some(path) } else { None }
}

fn hyprland_clients(socket_path: &PathBuf) -> Option<Vec<WindowInfo>> {
    let raw = hypr_query(socket_path, "j/clients")?;
    let clients: Value = serde_json::from_str(&raw).ok()?;
    let clients = clients.as_array()?;

    let mut output = Vec::new();

    for client in clients {
        let mapped = client
            .get("mapped")
            .and_then(|x| x.as_bool())
            .unwrap_or(true);
        let hidden = client
            .get("hidden")
            .and_then(|x| x.as_bool())
            .unwrap_or(false);
        if !mapped || hidden {
            continue;
        }

        output.push(hypr_window(client));
    }

    Some(output)
}

fn hyprland_active_window(socket_path: &PathBuf) -> Option<WindowInfo> {
    let raw = hypr_query(socket_path, "j/activewindow")?;
    let window: Value = serde_json::from_str(&raw).ok()?;
    Some(hypr_window(&window))
}

fn hyprland_monitors(socket_path: &PathBuf) -> Option<Vec<DisplayInfo>> {
    let raw = hypr_query(socket_path, "j/monitors")?;
    let monitors: Value = serde_json::from_str(&raw).ok()?;
    let monitors = monitors.as_array()?;

    Some(
        monitors
            .iter()
            .map(|monitor| {
                let x = monitor.get("x").and_then(|x| x.as_i64()).unwrap_or(0) as i32;
                let y = monitor.get("y").and_then(|x| x.as_i64()).unwrap_or(0) as i32;
                let w = monitor.get("width").and_then(|x| x.as_i64()).unwrap_or(0) as i32;
                let h = monitor.get("height").and_then(|x| x.as_i64()).unwrap_or(0) as i32;

                DisplayInfo {
                    id: monitor.get("id").and_then(|x| x.as_i64()).unwrap_or(0) as i32,
                    bounds: Bounds { x, y, w, h },
                    is_primary: monitor
                        .get("focused")
                        .and_then(|x| x.as_bool())
                        .unwrap_or(false),
                    scale_factor: monitor.get("scale").and_then(|x| x.as_f64()).unwrap_or(1.0),
                }
            })
            .collect(),
    )
}

fn hypr_window(value: &Value) -> WindowInfo {
    let bounds = hypr_bounds(value);

    let fullscreen = value
        .get("fullscreen")
        .and_then(|x| x.as_i64())
        .unwrap_or(0)
        > 0;

    let app = value
        .get("class")
        .and_then(|x| x.as_str())
        .unwrap_or("unknown")
        .to_string();

    let title = value
        .get("title")
        .and_then(|x| x.as_str())
        .unwrap_or("unknown")
        .to_string();

    let pid = value.get("pid").and_then(|x| x.as_u64()).unwrap_or(0) as u32;

    let workspace = value
        .get("workspace")
        .and_then(|x| x.get("id"))
        .and_then(|x| x.as_i64())
        .unwrap_or(0) as i32;

    let id = value
        .get("address")
        .and_then(|x| x.as_str())
        .unwrap_or("0x0")
        .to_string();

    WindowInfo {
        id,
        title,
        app,
        pid,
        bounds,
        workspace,
        is_minimized: false,
        is_fullscreen: fullscreen,
    }
}

fn hypr_bounds(value: &Value) -> Bounds {
    let at = value.get("at").and_then(|x| x.as_array());
    let size = value.get("size").and_then(|x| x.as_array());

    let x = at
        .and_then(|arr| arr.first())
        .and_then(|x| x.as_i64())
        .unwrap_or(0) as i32;
    let y = at
        .and_then(|arr| arr.get(1))
        .and_then(|x| x.as_i64())
        .unwrap_or(0) as i32;
    let w = size
        .and_then(|arr| arr.first())
        .and_then(|x| x.as_i64())
        .unwrap_or(0) as i32;
    let h = size
        .and_then(|arr| arr.get(1))
        .and_then(|x| x.as_i64())
        .unwrap_or(0) as i32;

    Bounds { x, y, w, h }
}

fn hypr_query(socket_path: &PathBuf, command: &str) -> Option<String> {
    let mut stream = UnixStream::connect(socket_path).ok()?;
    stream.write_all(command.as_bytes()).ok()?;
    stream.shutdown(std::net::Shutdown::Write).ok()?;

    let mut out = String::new();
    stream.read_to_string(&mut out).ok()?;

    if out.trim().is_empty() {
        None
    } else {
        Some(out)
    }
}

fn linux_process_cwd(pid: u32) -> Option<String> {
    fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .map(|path| path.display().to_string())
}

fn is_terminal_app(app: &str) -> bool {
    let app = app.to_ascii_lowercase();
    [
        "alacritty",
        "kitty",
        "wezterm",
        "gnome-terminal",
        "konsole",
        "xterm",
        "foot",
    ]
    .iter()
    .any(|name| app.contains(name))
}

fn os_release() -> std::collections::HashMap<String, String> {
    let mut values = std::collections::HashMap::new();
    let content = fs::read_to_string("/etc/os-release").unwrap_or_default();

    for line in content.lines() {
        if let Some((key, value)) = line.split_once('=') {
            values.insert(key.to_string(), value.trim_matches('"').trim().to_string());
        }
    }

    values
}

fn detect_container() -> bool {
    if fs::metadata("/.dockerenv").is_ok() {
        return true;
    }

    fs::read_to_string("/proc/1/cgroup")
        .map(|x| x.contains("docker") || x.contains("containerd") || x.contains("kubepods"))
        .unwrap_or(false)
}

fn chassis_from_dmi() -> Option<String> {
    let code = fs::read_to_string("/sys/class/dmi/id/chassis_type").ok()?;
    let code = code.trim().parse::<u32>().ok()?;

    let value = match code {
        8..=14 => "Laptop",
        3 | 4 | 5 | 6 | 7 | 15 | 16 => "Desktop",
        _ => "Unknown",
    };

    Some(value.to_string())
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
    let output = command_stdout("ip", &["route", "show", "default"])?;
    output.lines().find_map(|line| {
        let cols: Vec<&str> = line.split_whitespace().collect();
        let via_index = cols.iter().position(|value| *value == "via")?;
        cols.get(via_index + 1).map(|x| x.to_string())
    })
}

fn gpu_info() -> Vec<GpuInfo> {
    let output = match command_stdout("lspci", &[]) {
        Some(output) => output,
        None => return Vec::new(),
    };

    output
        .lines()
        .filter(|line| line.contains("VGA") || line.contains("3D controller"))
        .map(|line| GpuInfo {
            name: line
                .split(':')
                .next_back()
                .unwrap_or("unknown")
                .trim()
                .to_string(),
            vram_gb: None,
            driver: "unknown".to_string(),
        })
        .collect()
}

fn linux_uptime_seconds() -> Option<u64> {
    let content = fs::read_to_string("/proc/uptime").ok()?;
    let first = content.split_whitespace().next()?;
    first.parse::<f64>().ok().map(|x| x as u64)
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

    output.lines().filter_map(parse_who_line).collect()
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

    let from = cols
        .last()
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
