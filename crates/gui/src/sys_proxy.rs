use anyhow::{Context, Result};
use std::process::Command;
use std::sync::{LazyLock, Mutex};

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const REG_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings";
/// Local/LAN destinations the OS should never send to the proxy. Covers all of
/// 172.16.0.0/12 (172.16–172.31), not just 172.16.x.x.
const BYPASS_LIST: &str = "localhost;127.*;10.*;172.16.*;172.17.*;172.18.*;172.19.*;172.20.*;172.21.*;172.22.*;172.23.*;172.24.*;172.25.*;172.26.*;172.27.*;172.28.*;172.29.*;172.30.*;172.31.*;192.168.*;169.254.*;<local>";

#[derive(Debug, Clone)]
struct PrevSysProxy {
    enabled: bool,
    server: Option<String>,
    override_list: Option<String>,
}

static SAVED_PROXY: LazyLock<Mutex<Option<PrevSysProxy>>> = LazyLock::new(|| Mutex::new(None));

#[cfg(target_os = "windows")]
fn run_reg(args: &[&str]) -> Result<()> {
    let status = Command::new("reg")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .context("failed to invoke reg.exe")?;
    if !status.success() {
        anyhow::bail!("reg.exe exited with {}", status);
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn query_reg_value(value_name: &str) -> Option<String> {
    let out = Command::new("reg")
        .args(["query", REG_KEY, "/v", value_name])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with(value_name) {
            let mut parts = trimmed.splitn(3, char::is_whitespace);
            let _name = parts.next()?;
            let rest = parts.next()?.trim_start();
            // Skip REG_SZ / REG_DWORD token
            let val = if let Some(after_type) = rest.split_once(char::is_whitespace) {
                after_type.1.trim()
            } else {
                parts.next()?.trim()
            };
            return Some(val.to_string());
        }
    }
    None
}

#[cfg(target_os = "windows")]
fn snapshot_current_proxy() -> PrevSysProxy {
    let enabled = query_reg_value("ProxyEnable")
        .map(|v| v == "0x1" || v == "1")
        .unwrap_or(false);
    let server = query_reg_value("ProxyServer").filter(|s| !s.is_empty());
    let override_list = query_reg_value("ProxyOverride").filter(|s| !s.is_empty());
    PrevSysProxy {
        enabled,
        server,
        override_list,
    }
}

/// Enable Windows system proxy pointing to our mixed (HTTP + SOCKS5) port.
/// Saves the pre-existing user proxy settings once so `disable_system_proxy` restores them.
pub fn enable_system_proxy(proxy_addr: &str) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        if let Ok(mut guard) = SAVED_PROXY.lock()
            && guard.is_none()
        {
            *guard = Some(snapshot_current_proxy());
        }
        run_reg(&["add", REG_KEY, "/v", "ProxyServer", "/t", "REG_SZ", "/d", proxy_addr, "/f"])?;
        run_reg(&["add", REG_KEY, "/v", "ProxyOverride", "/t", "REG_SZ", "/d", BYPASS_LIST, "/f"])?;
        run_reg(&["add", REG_KEY, "/v", "ProxyEnable", "/t", "REG_DWORD", "/d", "1", "/f"])?;
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = proxy_addr;
    }
    Ok(())
}

/// Restore the pre-existing Windows system proxy (or disable it if none was active before).
pub fn disable_system_proxy() -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        let prev = SAVED_PROXY.lock().ok().and_then(|mut g| g.take());
        if let Some(p) = prev {
            if let Some(srv) = &p.server {
                let _ = run_reg(&["add", REG_KEY, "/v", "ProxyServer", "/t", "REG_SZ", "/d", srv, "/f"]);
            } else {
                let _ = run_reg(&["delete", REG_KEY, "/v", "ProxyServer", "/f"]);
            }
            if let Some(ov) = &p.override_list {
                let _ = run_reg(&["add", REG_KEY, "/v", "ProxyOverride", "/t", "REG_SZ", "/d", ov, "/f"]);
            } else {
                let _ = run_reg(&["delete", REG_KEY, "/v", "ProxyOverride", "/f"]);
            }
            let en = if p.enabled { "1" } else { "0" };
            run_reg(&["add", REG_KEY, "/v", "ProxyEnable", "/t", "REG_DWORD", "/d", en, "/f"])?;
        } else {
            run_reg(&["add", REG_KEY, "/v", "ProxyEnable", "/t", "REG_DWORD", "/d", "0", "/f"])?;
        }
    }
    Ok(())
}
