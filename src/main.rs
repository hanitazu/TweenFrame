#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! TweenFrame
//!
//! Dev flags:
//!   --headless <seconds>   run the engine without UI for N seconds
//!   --mult <1|2|3>         multiplier for --headless
//!   --title <text>         target a window by title instead of the RAGE class
//!   --ignore-foreground    keep the overlay up even if the game is not focused
//!   --debug-tint           tint generated frames magenta (to see the overlay work)
//!   --tray                 start hidden in the tray
//!   --quit                 close a running instance

use std::sync::atomic::Ordering;
use tweenframe::config::Config;
use tweenframe::engine::{self, Shared};
use tweenframe::target::Matcher;
use windows::core::w;
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};

fn main() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let mut cfg = Config::load();
    let mut headless: Option<f64> = None;
    let mut mult: Option<u32> = None;
    let mut ignore_fg = false;
    let mut tint = false;
    let mut tray = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--headless" => headless = args.next().and_then(|v| v.parse().ok()),
            "--mult" => mult = args.next().and_then(|v| v.parse().ok()),
            "--title" => cfg.title_contains = args.next(),
            "--ignore-foreground" => ignore_fg = true,
            "--debug-tint" => tint = true,
            "--tray" => tray = true,
            "--quit" => {
                tweenframe::ui::signal_existing_instance(tweenframe::ui::WM_APP_QUIT);
                return;
            }
            _ => {}
        }
    }
    let matcher = Matcher::new(&cfg.extra_classes, cfg.title_contains.clone());

    // Single instance: a second launch just opens the existing window (and must
    // not touch the running instance's log file).
    let _mutex = unsafe { CreateMutexW(None, true, w!("Local\\TweenFrame.SingleInstance")) };
    if headless.is_none() && unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        tweenframe::ui::signal_existing_instance(tweenframe::ui::WM_APP_SHOW);
        return;
    }
    tweenframe::log::init();
    tweenframe::log!("TweenFrame {} starting", env!("CARGO_PKG_VERSION"));

    if let Some(secs) = headless {
        let shared = Shared::new(mult.unwrap_or(3).clamp(1, 3), matcher);
        shared.ignore_foreground.store(ignore_fg, Ordering::Relaxed);
        shared.debug_tint.store(tint, Ordering::Relaxed);
        let handle = engine::spawn(shared.clone());
        std::thread::sleep(std::time::Duration::from_secs_f64(secs));
        shared.quit.store(true, Ordering::Relaxed);
        shared.wake();
        let _ = handle.join();
        tweenframe::log!("headless run finished");
        return;
    }

    // Frame generation starts only after the user presses Apply.
    let shared = Shared::new(1, matcher);
    shared.ignore_foreground.store(ignore_fg, Ordering::Relaxed);
    let handle = engine::spawn(shared.clone());
    if let Err(e) = tweenframe::ui::run(cfg, shared.clone(), tray) {
        tweenframe::log!("ui error: {e:?}");
    }
    shared.quit.store(true, Ordering::Relaxed);
    shared.wake();
    let _ = handle.join();
    tweenframe::log!("bye");
}
