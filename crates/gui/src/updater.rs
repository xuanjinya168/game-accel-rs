//! In-app updater backed by GitHub Releases.
//!
//! Flow: query `releases/latest` → if newer, download the Inno Setup installer asset plus
//! `SHA256SUMS.txt` → verify the hash → run the installer silently (it closes this app,
//! replaces files in Program Files and relaunches the new version).

use crate::logger::log_line;
use anyhow::{Context as _, Result, anyhow, bail};
use eframe::egui;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;

pub const REPO: &str = "xuanjinya168/game-accel-rs";
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// First automatic check shortly after startup, then periodically.
const FIRST_AUTO_CHECK_DELAY: Duration = Duration::from_secs(8);
const AUTO_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Debug, Clone)]
pub struct ReleaseInfo {
    pub version: String,
    pub html_url: String,
    pub installer_url: String,
    pub installer_name: String,
    pub expected_sha256: String,
}

#[derive(Debug, Clone, Default)]
pub enum UpdateStatus {
    #[default]
    Idle,
    Checking,
    UpToDate {
        checked_at: String,
    },
    Available(ReleaseInfo),
    Downloading {
        info: ReleaseInfo,
        downloaded: u64,
        total: Option<u64>,
    },
    Ready {
        info: ReleaseInfo,
        installer: PathBuf,
    },
    Failed(String),
}

impl UpdateStatus {
    pub fn is_busy(&self) -> bool {
        matches!(self, Self::Checking | Self::Downloading { .. })
    }
}

pub struct Updater {
    status: Arc<Mutex<UpdateStatus>>,
    next_auto_check: Instant,
}

impl Default for Updater {
    fn default() -> Self {
        Self {
            status: Arc::default(),
            next_auto_check: Instant::now() + FIRST_AUTO_CHECK_DELAY,
        }
    }
}

impl Updater {
    pub fn status(&self) -> UpdateStatus {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    fn set(status: &Arc<Mutex<UpdateStatus>>, ctx: &egui::Context, new: UpdateStatus) {
        if let Ok(mut s) = status.lock() {
            *s = new;
        }
        ctx.request_repaint_of(egui::ViewportId::ROOT);
    }

    /// Run the periodic background check if it's due. Returns quickly when not due.
    pub fn tick_auto_check(
        &mut self,
        enabled: bool,
        rt: &tokio::runtime::Runtime,
        ctx: &egui::Context,
    ) {
        if !enabled || Instant::now() < self.next_auto_check {
            return;
        }
        self.next_auto_check = Instant::now() + AUTO_CHECK_INTERVAL;
        // Don't clobber a pending download / ready installer / visible result.
        if matches!(
            self.status(),
            UpdateStatus::Idle | UpdateStatus::UpToDate { .. }
        ) {
            self.check(rt, ctx, true);
        }
    }

    /// Query GitHub for the latest release. `silent` checks don't surface network errors.
    pub fn check(&self, rt: &tokio::runtime::Runtime, ctx: &egui::Context, silent: bool) {
        if self.status().is_busy() {
            return;
        }
        let status = self.status.clone();
        let ctx = ctx.clone();
        let prev = self.status();
        Self::set(&status, &ctx, UpdateStatus::Checking);
        rt.spawn(async move {
            let new = match fetch_latest().await {
                Ok(Some(info)) => {
                    log_line(
                        "INFO",
                        &format!("发现新版本 v{}（当前 v{CURRENT_VERSION}）", info.version),
                    );
                    UpdateStatus::Available(info)
                }
                Ok(None) => UpdateStatus::UpToDate {
                    checked_at: chrono::Local::now().format("%H:%M").to_string(),
                },
                Err(e) => {
                    log_line("WARN", &format!("检查更新失败: {e:#}"));
                    if silent {
                        prev
                    } else {
                        UpdateStatus::Failed(format!("检查更新失败：{e:#}"))
                    }
                }
            };
            Self::set(&status, &ctx, new);
        });
    }

    /// Download and verify the installer for `info`.
    pub fn download(&self, rt: &tokio::runtime::Runtime, ctx: &egui::Context, info: ReleaseInfo) {
        if self.status().is_busy() {
            return;
        }
        let status = self.status.clone();
        let ctx = ctx.clone();
        Self::set(
            &status,
            &ctx,
            UpdateStatus::Downloading {
                info: info.clone(),
                downloaded: 0,
                total: None,
            },
        );
        rt.spawn(async move {
            let new = match download_and_verify(&info, &status, &ctx).await {
                Ok(installer) => {
                    log_line(
                        "INFO",
                        &format!("新版本安装包已下载并校验: {}", installer.display()),
                    );
                    UpdateStatus::Ready { info, installer }
                }
                Err(e) => {
                    log_line("WARN", &format!("下载更新失败: {e:#}"));
                    UpdateStatus::Failed(format!("下载更新失败：{e:#}"))
                }
            };
            Self::set(&status, &ctx, new);
        });
    }
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(format!("game-accel/{CURRENT_VERSION}"))
        .connect_timeout(Duration::from_secs(15))
        .build()
        .context("创建 HTTP 客户端失败")
}

/// Query the latest release without calling `api.github.com`, avoiding its 60 req/hr
/// per-IP unauthenticated rate limit (easily hit by users sharing a proxy exit IP).
async fn fetch_latest() -> Result<Option<ReleaseInfo>> {
    let no_redirect = reqwest::Client::builder()
        .user_agent(format!("game-accel/{CURRENT_VERSION}"))
        .connect_timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("创建 HTTP 客户端失败")?;

    let latest_url = format!("https://github.com/{REPO}/releases/latest");
    let resp = tokio::time::timeout(Duration::from_secs(20), no_redirect.head(&latest_url).send())
        .await
        .context("连接 GitHub 超时")??;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_redirection() {
        bail!("GitHub 返回异常状态码 {}", resp.status());
    }
    let location = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| anyhow!("GitHub 重定向缺少 Location 头"))?;
    let Some(tag) = parse_tag_from_location(location) else {
        // E.g. redirected to `/releases` when no release has been published yet.
        return Ok(None);
    };

    let version = tag.trim_start_matches('v').to_string();
    if !is_newer(&version, CURRENT_VERSION) {
        return Ok(None);
    }

    let client = http_client()?;
    let sums_url = format!("https://github.com/{REPO}/releases/download/{tag}/SHA256SUMS.txt");
    let sums_resp = tokio::time::timeout(Duration::from_secs(20), client.get(&sums_url).send())
        .await
        .context("获取 SHA256SUMS.txt 超时")??;
    if sums_resp.status() == reqwest::StatusCode::NOT_FOUND {
        bail!("新版本 v{version} 缺少 SHA256SUMS.txt 校验文件");
    }
    let sums = sums_resp
        .error_for_status()
        .context("下载 SHA256SUMS.txt 失败")?
        .text()
        .await
        .context("读取 SHA256SUMS.txt 失败")?;

    let (installer_name, expected_sha256) =
        find_asset_in_sha256sums(&sums, "-windows-x64-setup.exe")
            .ok_or_else(|| anyhow!("新版本 v{version} 没有提供 Windows 安装包"))?;

    Ok(Some(ReleaseInfo {
        version,
        html_url: format!("https://github.com/{REPO}/releases/tag/{tag}"),
        installer_url: format!(
            "https://github.com/{REPO}/releases/download/{tag}/{installer_name}"
        ),
        installer_name,
        expected_sha256,
    }))
}

async fn download_and_verify(
    info: &ReleaseInfo,
    status: &Arc<Mutex<UpdateStatus>>,
    ctx: &egui::Context,
) -> Result<PathBuf> {
    let client = http_client()?;

    let dir = std::env::temp_dir().join("game-accel-update");
    tokio::fs::create_dir_all(&dir)
        .await
        .context("创建临时目录失败")?;
    let final_path = dir.join(&info.installer_name);
    let part_path = dir.join(format!("{}.part", info.installer_name));

    let mut resp = client
        .get(&info.installer_url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .context("下载安装包失败")?;
    let total = resp.content_length();
    let mut file = tokio::fs::File::create(&part_path)
        .await
        .context("创建临时文件失败")?;
    let mut hasher = ring::digest::Context::new(&ring::digest::SHA256);
    let mut downloaded: u64 = 0;
    let mut last_report = Instant::now();

    loop {
        let chunk = tokio::time::timeout(Duration::from_secs(30), resp.chunk())
            .await
            .context("下载超时（30 秒无数据）")?
            .context("下载中断")?;
        let Some(chunk) = chunk else { break };
        hasher.update(&chunk);
        file.write_all(&chunk).await.context("写入临时文件失败")?;
        downloaded += chunk.len() as u64;
        if last_report.elapsed() >= Duration::from_millis(150) {
            last_report = Instant::now();
            Updater::set(
                status,
                ctx,
                UpdateStatus::Downloading {
                    info: info.clone(),
                    downloaded,
                    total,
                },
            );
        }
    }
    file.flush().await.ok();
    drop(file);

    if let Some(t) = total
        && t != downloaded
    {
        let _ = tokio::fs::remove_file(&part_path).await;
        bail!("下载不完整（{downloaded}/{t} 字节）");
    }
    let actual = hex(hasher.finish().as_ref());
    if !actual.eq_ignore_ascii_case(&info.expected_sha256) {
        let _ = tokio::fs::remove_file(&part_path).await;
        bail!("安装包校验失败（SHA256 不匹配），已丢弃");
    }

    let _ = tokio::fs::remove_file(&final_path).await;
    tokio::fs::rename(&part_path, &final_path)
        .await
        .context("保存安装包失败")?;
    Ok(final_path)
}

/// Run the downloaded installer silently. Inno Setup closes this app (Restart Manager),
/// upgrades the install in place and relaunches the new version — elevated only if this
/// instance was elevated (`relaunch_admin`), otherwise as the normal user.
pub fn launch_installer(installer: &std::path::Path, relaunch_admin: bool) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "shell32")]
        unsafe extern "system" {
            fn ShellExecuteW(
                hwnd: isize,
                op: *const u16,
                file: *const u16,
                params: *const u16,
                dir: *const u16,
                show: i32,
            ) -> isize;
        }
        let wide = |s: &std::ffi::OsStr| s.encode_wide().chain(Some(0)).collect::<Vec<u16>>();
        let op = wide("open".as_ref());
        let file = wide(installer.as_os_str());
        let mut args = "/SILENT /SUPPRESSMSGBOXES /NORESTART /CLOSEAPPLICATIONS".to_string();
        if relaunch_admin {
            args.push_str(" /relaunchadmin=1");
        }
        let params = wide(args.as_ref());
        const SW_SHOWNORMAL: i32 = 1;
        // ShellExecute (not CreateProcess) so a UAC prompt is shown when we're not elevated.
        let rc = unsafe {
            ShellExecuteW(
                0,
                op.as_ptr(),
                file.as_ptr(),
                params.as_ptr(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            )
        };
        if rc <= 32 {
            bail!("启动安装程序失败（错误码 {rc}，可能取消了管理员授权）");
        }
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (installer, relaunch_admin);
        bail!("仅支持 Windows 自动更新")
    }
}

/// Whether this exe runs from the installed location (Program Files).
pub fn is_installed_copy() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let exe = exe.to_string_lossy().to_lowercase();
    ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .any(|dir| exe.starts_with(&dir.to_lowercase()))
}

pub fn open_url(url: &str) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", url])
            .creation_flags(0x0800_0000)
            .spawn();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = url;
    }
}

/// Extract the release tag from a `releases/latest` redirect `Location` URL, e.g.
/// `https://github.com/owner/repo/releases/tag/v0.2.4` -> `Some("v0.2.4")`.
fn parse_tag_from_location(location: &str) -> Option<String> {
    let rest = location.split_once("/releases/tag/")?.1;
    let tag = rest
        .split(['?', '#', '/'])
        .next()
        .unwrap_or("")
        .trim();
    (!tag.is_empty()).then(|| tag.to_string())
}

/// Find the first `(filename, sha256)` entry in `SHA256SUMS.txt` whose filename ends with `suffix`.
fn find_asset_in_sha256sums(text: &str, suffix: &str) -> Option<(String, String)> {
    text.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let name = parts.next()?.trim_start_matches('*');
        let valid_hash = hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit());
        (valid_hash && name.ends_with(suffix)).then(|| (name.to_string(), hash.to_string()))
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Numeric dotted-version comparison; pre-release suffixes (`-beta`) are ignored.
fn is_newer(candidate: &str, current: &str) -> bool {
    fn parts(v: &str) -> Vec<u64> {
        v.split(['-', '+'])
            .next()
            .unwrap_or("")
            .split('.')
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    }
    let (mut a, mut b) = (parts(candidate), parts(current));
    let n = a.len().max(b.len());
    a.resize(n, 0);
    b.resize(n, 0);
    a > b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare() {
        assert!(is_newer("0.2.3", "0.2.2"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("1.0", "0.9.9"));
        assert!(!is_newer("0.2.2", "0.2.2"));
        assert!(!is_newer("0.2.1", "0.2.2"));
        assert!(!is_newer("0.2.2-beta", "0.2.2"));
    }

    #[test]
    fn sums_parsing() {
        let h = "a".repeat(64);
        let text = format!("{h}  game-accel-v0.2.3-windows-x64-setup.exe\n{h} *other.zip\n");
        assert_eq!(
            find_asset_in_sha256sums(&text, "-windows-x64-setup.exe"),
            Some(("game-accel-v0.2.3-windows-x64-setup.exe".into(), h.clone()))
        );
        assert_eq!(
            find_asset_in_sha256sums(&text, ".zip"),
            Some(("other.zip".into(), h))
        );
        assert_eq!(find_asset_in_sha256sums(&text, ".msi"), None);
    }

    #[test]
    fn redirect_tag_parsing() {
        assert_eq!(
            parse_tag_from_location("https://github.com/xuanjinya168/game-accel-rs/releases/tag/v0.2.4"),
            Some("v0.2.4".into())
        );
        assert_eq!(
            parse_tag_from_location("/xuanjinya168/game-accel-rs/releases/tag/v1.0.0?foo=1"),
            Some("v1.0.0".into())
        );
        assert_eq!(
            parse_tag_from_location("https://github.com/xuanjinya168/game-accel-rs/releases"),
            None
        );
    }
}
