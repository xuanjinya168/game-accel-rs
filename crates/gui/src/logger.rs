use crate::storage::config_dir;
use chrono::Local;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

static LOG_LOCK: Mutex<()> = Mutex::new(());

pub fn log_file_path() -> PathBuf {
    config_dir().join("logs").join("app.log")
}

/// Captures `tracing::warn!` / `tracing::error!` events from `client`, `protocol`,
/// `quinn`, and `netstack_smoltcp` into `app.log` atomically under `LOG_LOCK`.
struct AppLogMakeWriter;

struct AppLogEventWriter(Vec<u8>);

impl<'a> MakeWriter<'a> for AppLogMakeWriter {
    type Writer = AppLogEventWriter;

    fn make_writer(&'a self) -> Self::Writer {
        AppLogEventWriter(Vec::with_capacity(160))
    }
}

impl Write for AppLogEventWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for AppLogEventWriter {
    fn drop(&mut self) {
        if self.0.is_empty() {
            return;
        }
        let text = String::from_utf8_lossy(&self.0);
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        let _guard = LOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = log_file_path();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
            let ts = Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
            let _ = writeln!(file, "[{ts}] [TRACE] {trimmed}");
            let _ = file.flush();
        }
    }
}

/// Initialize persistent disk logging and install a global panic hook
/// so any crash or error is captured in `%APPDATA%\game-accel\logs\app.log`.
pub fn init_logger() {
    let path = log_file_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    // Simple size rotation (> 5 MB -> app.log.old)
    if let Ok(meta) = fs::metadata(&path)
        && meta.len() > 5 * 1024 * 1024
    {
        let old = path.with_extension("log.old");
        let _ = fs::rename(&path, old);
    }

    // Route `tracing` warnings/errors from client/protocol/netstack into `app.log`
    // (default `warn` avoids duplicating user-facing `INFO` lines emitted via `log_tx`).
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(AppLogMakeWriter)
        .with_ansi(false)
        .without_time()
        .with_target(true)
        .try_init();

    log_line("INFO", "========== GameAccel 启动 ==========");

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown".to_string());
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(|s| s.as_str()))
            .unwrap_or("unknown panic payload");
        log_line("PANIC", &format!("at {location}: {msg}"));
        default_hook(info);
    }));
}

pub fn log_line(level: &str, msg: &str) {
    let _guard = LOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = log_file_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let ts = Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
        let _ = writeln!(file, "[{ts}] [{level}] {msg}");
        let _ = file.flush();
    }
}
