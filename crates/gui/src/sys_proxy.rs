use anyhow::{Context, Result};
use std::process::Command;

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const REG_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings";
const BYPASS_LIST: &str = "localhost;127.*;10.*;172.16.*;192.168.*;<local>";

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

/// Enable Windows system proxy pointing to our mixed (HTTP + SOCKS5) port.
/// Using plain `ip:port` is universally supported by Chrome, Edge, Electron, curl, and Windows apps.
pub fn enable_system_proxy(proxy_addr: &str) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
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

/// Disable Windows system proxy cleanly on disconnect or app exit.
pub fn disable_system_proxy() -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        run_reg(&["add", REG_KEY, "/v", "ProxyEnable", "/t", "REG_DWORD", "/d", "0", "/f"])?;
    }
    Ok(())
}
