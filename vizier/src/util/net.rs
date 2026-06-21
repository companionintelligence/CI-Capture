#[cfg(target_os = "macos")]
use std::collections::HashSet;
#[cfg(target_os = "linux")]
use std::collections::HashSet as LinuxHashSet;
use std::process::Command;

use crate::observation::{ConnInfo, ListeningPort};

pub fn collect_active_connections(all_connections: bool) -> Vec<ConnInfo> {
    #[cfg(target_os = "macos")]
    {
        parse_established_lsof(all_connections)
    }

    #[cfg(target_os = "linux")]
    {
        parse_established_ss(all_connections)
    }

    #[cfg(all(not(target_os = "macos"), not(target_os = "linux")))]
    {
        let _ = all_connections;
        Vec::new()
    }
}

pub fn collect_listening_ports() -> Vec<ListeningPort> {
    #[cfg(target_os = "macos")]
    {
        parse_listening_lsof()
    }

    #[cfg(target_os = "linux")]
    {
        parse_listening_ss()
    }

    #[cfg(all(not(target_os = "macos"), not(target_os = "linux")))]
    {
        Vec::new()
    }
}

#[cfg(target_os = "macos")]
fn parse_established_lsof(all_connections: bool) -> Vec<ConnInfo> {
    let output = match run_command("lsof", &["-nP", "-iTCP", "-sTCP:ESTABLISHED"]) {
        Some(output) => output,
        None => return Vec::new(),
    };

    let mut seen = HashSet::new();

    output
        .lines()
        .skip(1)
        .filter_map(|line| parse_established_line(line, all_connections))
        .filter(|conn| {
            let key = format!(
                "{}:{}:{}:{}:{}",
                conn.app, conn.pid, conn.local_port, conn.remote_addr, conn.remote_port
            );
            seen.insert(key)
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn parse_listening_lsof() -> Vec<ListeningPort> {
    let output = match run_command("lsof", &["-nP", "-iTCP", "-sTCP:LISTEN"]) {
        Some(output) => output,
        None => return Vec::new(),
    };

    let mut seen = HashSet::new();

    output
        .lines()
        .skip(1)
        .filter_map(parse_listen_line)
        .filter(|port| {
            let key = format!("{}:{}:{}:{}", port.app, port.pid, port.addr, port.port);
            seen.insert(key)
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn parse_established_ss(all_connections: bool) -> Vec<ConnInfo> {
    let output = match run_command("ss", &["-ntpH"]) {
        Some(output) => output,
        None => return Vec::new(),
    };

    let mut seen = LinuxHashSet::new();

    output
        .lines()
        .filter_map(|line| parse_ss_established_line(line, all_connections))
        .filter(|conn| {
            let key = format!(
                "{}:{}:{}:{}:{}",
                conn.app, conn.pid, conn.local_port, conn.remote_addr, conn.remote_port
            );
            seen.insert(key)
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn parse_listening_ss() -> Vec<ListeningPort> {
    let output = match run_command("ss", &["-lntpH"]) {
        Some(output) => output,
        None => return Vec::new(),
    };

    let mut seen = LinuxHashSet::new();

    output
        .lines()
        .filter_map(parse_ss_listen_line)
        .filter(|port| {
            let key = format!("{}:{}:{}:{}", port.app, port.pid, port.addr, port.port);
            seen.insert(key)
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn parse_established_line(line: &str, all_connections: bool) -> Option<ConnInfo> {
    let cols: Vec<&str> = line.split_whitespace().collect();
    if cols.len() < 9 {
        return None;
    }

    let endpoint = cols.iter().find(|x| x.contains("->"))?;
    let endpoint = endpoint
        .strip_suffix("(ESTABLISHED)")
        .unwrap_or(endpoint)
        .trim();
    let (local, remote) = endpoint.split_once("->")?;
    let (local_addr, local_port) = parse_host_port(local)?;
    let (remote_addr, remote_port) = parse_host_port(remote)?;

    if !all_connections && (is_loopback_addr(&local_addr) || is_loopback_addr(&remote_addr)) {
        return None;
    }

    let pid = cols.get(1)?.parse::<u32>().ok()?;
    let app = cols.first()?.to_string();

    Some(ConnInfo {
        proto: "tcp".to_string(),
        local_port,
        remote_addr,
        remote_port,
        pid,
        app,
        state: "ESTABLISHED".to_string(),
        connection_count: None,
        remote_host_count: None,
    })
}

#[cfg(target_os = "linux")]
fn parse_ss_established_line(line: &str, all_connections: bool) -> Option<ConnInfo> {
    let cols: Vec<&str> = line.split_whitespace().collect();
    if cols.len() < 6 {
        return None;
    }

    if cols[0] != "ESTAB" {
        return None;
    }

    let local = cols[3];
    let remote = cols[4];
    let (local_addr, local_port) = parse_host_port(local)?;
    let (remote_addr, remote_port) = parse_host_port(remote)?;

    if !all_connections && (is_loopback_addr(&local_addr) || is_loopback_addr(&remote_addr)) {
        return None;
    }

    let (app, pid) = parse_ss_process(cols.get(5).copied().unwrap_or_default());

    Some(ConnInfo {
        proto: "tcp".to_string(),
        local_port,
        remote_addr,
        remote_port,
        pid,
        app,
        state: "ESTABLISHED".to_string(),
        connection_count: None,
        remote_host_count: None,
    })
}

#[cfg(target_os = "macos")]
fn parse_listen_line(line: &str) -> Option<ListeningPort> {
    let cols: Vec<&str> = line.split_whitespace().collect();
    if cols.len() < 9 {
        return None;
    }

    let endpoint = cols.iter().find(|x| x.contains(':'))?;
    let endpoint = endpoint.strip_suffix("(LISTEN)").unwrap_or(endpoint).trim();
    let (addr, port) = parse_host_port(endpoint)?;

    let pid = cols.get(1)?.parse::<u32>().ok()?;
    let app = cols.first()?.to_string();

    Some(ListeningPort {
        port,
        proto: "tcp".to_string(),
        pid,
        app,
        addr,
    })
}

#[cfg(target_os = "linux")]
fn parse_ss_listen_line(line: &str) -> Option<ListeningPort> {
    let cols: Vec<&str> = line.split_whitespace().collect();
    if cols.len() < 5 {
        return None;
    }

    let local = cols[3];
    let (addr, port) = parse_host_port(local)?;
    let (app, pid) = parse_ss_process(cols.get(5).copied().unwrap_or_default());

    Some(ListeningPort {
        port,
        proto: "tcp".to_string(),
        pid,
        app,
        addr,
    })
}

#[cfg(target_os = "linux")]
fn parse_ss_process(input: &str) -> (String, u32) {
    let name = input
        .split('"')
        .nth(1)
        .map(|x| x.to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let pid = input
        .split("pid=")
        .nth(1)
        .and_then(|x| x.split(',').next())
        .and_then(|x| x.parse::<u32>().ok())
        .unwrap_or(0);

    (name, pid)
}

fn run_command(bin: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(bin).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

fn parse_host_port(input: &str) -> Option<(String, u16)> {
    let trimmed = input.trim().trim_start_matches('[').trim_end_matches(']');

    if let Some((host, port)) = trimmed.rsplit_once(':') {
        return Some((normalize_host(host), port.parse::<u16>().ok()?));
    }

    None
}

fn normalize_host(input: &str) -> String {
    input.trim_matches(['[', ']']).to_string()
}

fn is_loopback_addr(addr: &str) -> bool {
    addr == "localhost"
        || addr == "::1"
        || addr.starts_with("127.")
        || addr.starts_with("fe80::1%")
        || addr == "*"
}
