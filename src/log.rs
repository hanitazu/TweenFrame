//! Tiny file logger. Writes to `%APPDATA%\TweenFrame\log.txt` (truncated on start).

use std::io::Write;
use std::sync::Mutex;

static LOG: Mutex<Option<std::fs::File>> = Mutex::new(None);

pub fn init() {
    let dir = crate::config::app_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(f) = std::fs::File::create(dir.join("log.txt")) {
        *LOG.lock().unwrap() = Some(f);
    }
}

pub fn write(msg: &str) {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    let line = format!("{:02}:{:02}:{:02}.{:03} {}\n", t.wHour, t.wMinute, t.wSecond, t.wMilliseconds, msg);
    if cfg!(debug_assertions) {
        eprint!("{line}");
    }
    if let Ok(mut guard) = LOG.lock() {
        if let Some(f) = guard.as_mut() {
            let _ = f.write_all(line.as_bytes());
            let _ = f.flush();
        }
    }
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => { $crate::log::write(&format!($($arg)*)) };
}
