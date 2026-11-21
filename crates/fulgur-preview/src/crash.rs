//! Panic hook that records crashes to `crash.log`.
//!
//! Release builds run without a console (`windows_subsystem = "windows"`), so a panic would
//! otherwise vanish without a trace. The hook appends a report to
//! `%LOCALAPPDATA%\FulgurPreView\crash.log` (or the platform equivalent) and, when the UI
//! thread itself panics, tells the user where the report is.
//!
//! Panics in background workers are logged only: the frame cache records the failed job and
//! the UI keeps running.
//!
//! The report contains the panic message, source location, thread name and a backtrace. It
//! never includes image headers, so no `SITELAT` / `SITELONG` data can leak into it.

use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Name of the crash report file inside the application data directory.
pub const CRASH_LOG_NAME: &str = "crash.log";

/// Crash log files larger than this are truncated before appending (1 MiB).
const MAX_LOG_BYTES: u64 = 1024 * 1024;

/// Directory for FulgurPreView's own files (`settings.json`, `crash.log`).
pub fn app_data_dir() -> PathBuf {
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join("Library").join("Application Support"));
    #[cfg(all(not(windows), not(target_os = "macos")))]
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    base.unwrap_or_else(std::env::temp_dir)
        .join("FulgurPreView")
}

/// Full path of the crash log.
pub fn crash_log_path() -> PathBuf {
    app_data_dir().join(CRASH_LOG_NAME)
}

/// Install the crash-logging panic hook. The previous (default) hook still runs, so debug
/// builds keep printing panics to the console.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let thread_name = thread.name().unwrap_or("<unnamed>").to_owned();
        let message = panic_message(info.payload());
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_owned());
        let backtrace = std::backtrace::Backtrace::force_capture();
        let report = format_report(&thread_name, &message, &location, &backtrace.to_string());
        let written = append_report(&report);

        if thread_name == "main" {
            notify_user(written.as_ref().ok());
        }
        previous(info);
    }));
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_owned()
    }
}

/// Build one crash report entry.
pub fn format_report(thread: &str, message: &str, location: &str, backtrace: &str) -> String {
    format!(
        "==== FulgurPreView {} crash at {} UTC ====\nthread: {thread}\nlocation: {location}\nmessage: {message}\nbacktrace:\n{backtrace}\n\n",
        crate::VERSION,
        utc_timestamp(SystemTime::now()),
    )
}

fn append_report(report: &str) -> std::io::Result<PathBuf> {
    let path = crash_log_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let too_big = std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_LOG_BYTES);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(!too_big)
        .write(true)
        .truncate(too_big)
        .open(&path)?;
    file.write_all(report.as_bytes())?;
    file.flush()?;
    Ok(path)
}

#[cfg(windows)]
fn notify_user(log_path: Option<&PathBuf>) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    let text = match log_path {
        Some(p) => format!(
            "FulgurPreView hit an internal error and must close.\n\nA report was saved to:\n{}",
            p.display()
        ),
        None => "FulgurPreView hit an internal error and must close.".to_owned(),
    };
    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain([0]).collect() };
    let text_w = wide(&text);
    let title_w = wide("FulgurPreView");
    // SAFETY: both strings are valid NUL-terminated UTF-16 buffers alive for the call;
    // a null owner window is allowed.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text_w.as_ptr(),
            title_w.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

#[cfg(not(windows))]
fn notify_user(log_path: Option<&PathBuf>) {
    if let Some(p) = log_path {
        eprintln!("FulgurPreView crashed; report saved to {}", p.display());
    }
}

/// Format a `SystemTime` as `YYYY-MM-DD HH:MM:SS` (UTC) without external crates.
pub fn utc_timestamp(time: SystemTime) -> String {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn utc_timestamp_matches_known_dates() {
        assert_eq!(utc_timestamp(UNIX_EPOCH), "1970-01-01 00:00:00");
        // 2026-11-16 13:51:01 UTC
        let t = UNIX_EPOCH + Duration::from_secs(1_794_837_061);
        assert_eq!(utc_timestamp(t), "2026-11-16 13:51:01");
        // Leap day 2024-02-29 12:00:00 UTC
        let t = UNIX_EPOCH + Duration::from_secs(1_709_208_000);
        assert_eq!(utc_timestamp(t), "2024-02-29 12:00:00");
    }

    #[test]
    fn report_contains_essentials() {
        let r = format_report("main", "boom", "src/x.rs:1:2", "bt");
        assert!(r.contains("thread: main"));
        assert!(r.contains("message: boom"));
        assert!(r.contains("location: src/x.rs:1:2"));
        assert!(r.contains(crate::VERSION));
    }
}
