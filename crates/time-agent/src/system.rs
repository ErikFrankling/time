//! The machine's own state each minute: locked, lid, power, network.
//!
//! All from files and small commands that cost nothing and need no extra
//! permission. Nothing here touches location: the network is identified by
//! its Wi-Fi name, its router's address and its DNS search domain, which is
//! enough to tell home from KTH.

use std::process::Command;

use time_core::event::Network;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SystemSample {
    pub locked: Option<bool>,
    pub lid_closed: Option<bool>,
    pub charging: Option<bool>,
    pub battery_percent: Option<u8>,
    pub network: Network,
}

pub trait System: Send {
    fn sample(&mut self) -> SystemSample;
}

/// Linux (Hyprland desktop or laptop).
#[derive(Default)]
pub struct Linux;

impl System for Linux {
    fn sample(&mut self) -> SystemSample {
        let (charging, battery_percent) = power();
        SystemSample {
            locked: Some(locked()),
            lid_closed: lid_closed(),
            charging,
            battery_percent,
            network: Network {
                ssid: ssid(),
                gateway: gateway(),
                domain: search_domain(),
            },
        }
    }
}

/// A screen locker is running.
fn locked() -> bool {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return false;
    };
    dir.flatten().any(|e| {
        std::fs::read_to_string(e.path().join("comm"))
            .map(|c| matches!(c.trim(), "hyprlock" | "swaylock" | "gtklock" | "waylock"))
            .unwrap_or(false)
    })
}

fn lid_closed() -> Option<bool> {
    let dir = std::fs::read_dir("/proc/acpi/button/lid").ok()?;
    for e in dir.flatten() {
        if let Ok(s) = std::fs::read_to_string(e.path().join("state")) {
            return Some(s.contains("closed"));
        }
    }
    None
}

/// (charging or full on mains, battery percent); both None without a battery.
fn power() -> (Option<bool>, Option<u8>) {
    let Ok(dir) = std::fs::read_dir("/sys/class/power_supply") else {
        return (None, None);
    };
    let read = |p: &std::path::Path, f: &str| {
        std::fs::read_to_string(p.join(f))
            .map(|s| s.trim().to_owned())
            .ok()
    };
    let (mut battery, mut status, mut mains) = (None, None, None);
    for e in dir.flatten() {
        let p = e.path();
        match read(&p, "type").as_deref() {
            Some("Battery") if read(&p, "scope").as_deref() != Some("Device") => {
                battery = read(&p, "capacity").and_then(|c| c.parse().ok());
                status = read(&p, "status");
            }
            Some("Mains") => mains = read(&p, "online").map(|o| o == "1"),
            _ => {}
        }
    }
    if battery.is_none() {
        return (None, None);
    }
    let charging = match status.as_deref() {
        Some("Charging") | Some("Full") => Some(true),
        Some("Discharging") => Some(false),
        _ => mains,
    };
    (charging, battery)
}

/// The default route's gateway, from /proc/net/route.
pub fn gateway() -> Option<String> {
    parse_route(&std::fs::read_to_string("/proc/net/route").ok()?)
}

pub fn parse_route(table: &str) -> Option<String> {
    table.lines().skip(1).find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() > 2 && f[1] == "00000000" {
            let g = u32::from_str_radix(f[2], 16).ok()?;
            let b = g.to_le_bytes();
            Some(format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3]))
        } else {
            None
        }
    })
}

fn search_domain() -> Option<String> {
    let conf = std::fs::read_to_string("/etc/resolv.conf").ok()?;
    conf.lines()
        .find_map(|l| {
            l.strip_prefix("search ")
                .or_else(|| l.strip_prefix("domain "))
        })
        .and_then(|s| s.split_whitespace().next())
        .map(str::to_owned)
}

/// The connected Wi-Fi network's name, if any.
fn ssid() -> Option<String> {
    let run = |cmd: &str, args: &[&str]| {
        Command::new(cmd)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    };
    if let Some(s) = run("iwgetid", &["-r"])
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
    {
        return Some(s);
    }
    run("nmcli", &["-t", "-f", "active,ssid", "dev", "wifi"])?
        .lines()
        .find_map(|l| l.strip_prefix("yes:").or_else(|| l.strip_prefix("ja:")))
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
}

/// For machines and tests with nothing to report.
pub struct NoSystem;

impl System for NoSystem {
    fn sample(&mut self) -> SystemSample {
        SystemSample::default()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn default_gateway_from_the_route_table() {
        let table =
            "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
                     eno1\t0032A8C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0\n\
                     eno1\t00000000\t0132A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n";
        assert_eq!(super::parse_route(table).as_deref(), Some("192.168.50.1"));
    }
}
