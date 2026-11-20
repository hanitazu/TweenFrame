//! The engine thread: waits for the game, captures it, generates frames and
//! paces them onto the display.
//!
//! Timing model: every real frame carries its QPC timestamp. The output for
//! "now" shows the scene as it was at `now - delay`, where `delay` is one game
//! frame plus capture latency plus a small jitter margin. That keeps the
//! generated frames exactly evenly spaced in time even when the game's frame
//! rate fluctuates, at the unavoidable cost of about one game frame of latency.

use crate::capture::Capture;
use crate::framegen::{FrameGen, Shown};
use crate::gpu::{now_100ns, Gpu};
use crate::overlay::Overlay;
use crate::target::{self, Matcher};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use windows::core::{Result, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, WAIT_OBJECT_0, WPARAM};
use windows::Win32::Graphics::Direct3D11::D3D11_BOX;
use windows::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTONEAREST};
use windows::Win32::System::Threading::*;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
use windows::Win32::UI::WindowsAndMessaging::*;

/// Posted to the UI window whenever status or statistics change.
pub const WM_APP_ENGINE: u32 = WM_APP + 10;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Status {
    /// Frame generation disabled (1x).
    Off = 0,
    /// Enabled, game not running.
    Waiting = 1,
    /// Generating frames on screen.
    Active = 2,
    /// Game running but not in the foreground / minimised.
    Paused = 3,
    /// Something failed; retrying.
    Error = 4,
    /// The game already runs at (almost) the monitor's refresh rate, so
    /// generated frames could not be shown anyway: stay out of the way.
    NotNeeded = 5,
}

impl Status {
    fn from_u32(v: u32) -> Status {
        match v {
            1 => Status::Waiting,
            2 => Status::Active,
            3 => Status::Paused,
            4 => Status::Error,
            5 => Status::NotNeeded,
            _ => Status::Off,
        }
    }
}

/// Above this share of the refresh rate frame generation is switched off...
const BYPASS_ENTER: f64 = 0.90;
/// ...and below this share it comes back (hysteresis against flicker).
const BYPASS_LEAVE: f64 = 0.80;

/// Refresh rate of the monitor (Hz), 60 if unknown.
fn refresh_hz(monitor: windows::Win32::Graphics::Gdi::HMONITOR) -> f64 {
    use windows::Win32::Graphics::Gdi::*;
    unsafe {
        let mut mi = MONITORINFOEXW::default();
        mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if !GetMonitorInfoW(monitor, &mut mi as *mut MONITORINFOEXW as *mut MONITORINFO).as_bool() {
            return 60.0;
        }
        let mut dm = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
        if !EnumDisplaySettingsW(PCWSTR(mi.szDevice.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm).as_bool() {
            return 60.0;
        }
        if dm.dmDisplayFrequency > 1 {
            dm.dmDisplayFrequency as f64
        } else {
            60.0
        }
    }
}

pub struct Shared {
    /// Applied multiplier: 1 = off, 2 = 2x, 3 = 3x.
    pub multiplier: AtomicU32,
    status: AtomicU32,
    pub base_fps_x10: AtomicU32,
    pub out_fps_x10: AtomicU32,
    /// Refresh rate of the game's monitor (Hz).
    pub refresh_hz: AtomicU32,
    pub quit: AtomicBool,
    pub notify_hwnd: AtomicIsize,
    pub ignore_foreground: AtomicBool,
    pub debug_tint: AtomicBool,
    pub matcher: Mutex<Matcher>,
    wake: isize,
}

impl Shared {
    pub fn new(multiplier: u32, matcher: Matcher) -> Arc<Shared> {
        let wake = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }.map(|h| h.0 as isize).unwrap_or(0);
        Arc::new(Shared {
            multiplier: AtomicU32::new(multiplier),
            status: AtomicU32::new(Status::Off as u32),
            base_fps_x10: AtomicU32::new(0),
            out_fps_x10: AtomicU32::new(0),
            refresh_hz: AtomicU32::new(60),
            quit: AtomicBool::new(false),
            notify_hwnd: AtomicIsize::new(0),
            ignore_foreground: AtomicBool::new(false),
            debug_tint: AtomicBool::new(false),
            matcher: Mutex::new(matcher),
            wake,
        })
    }

    fn wake_handle(&self) -> HANDLE {
        HANDLE(self.wake as *mut _)
    }

    /// Wakes the engine thread (settings changed / quit).
    pub fn wake(&self) {
        unsafe {
            let _ = SetEvent(self.wake_handle());
        }
    }

    fn wait_wake(&self, ms: u32) {
        unsafe {
            WaitForSingleObject(self.wake_handle(), ms);
        }
    }

    pub fn status(&self) -> Status {
        Status::from_u32(self.status.load(Ordering::Relaxed))
    }

    fn set_status(&self, s: Status) {
        if self.status.swap(s as u32, Ordering::Relaxed) != s as u32 {
            crate::log!("status -> {s:?}");
            if s != Status::Active {
                self.out_fps_x10.store(0, Ordering::Relaxed);
            }
            if matches!(s, Status::Off | Status::Waiting | Status::Error) {
                self.base_fps_x10.store(0, Ordering::Relaxed);
            }
            self.notify();
        }
    }

    fn notify(&self) {
        let h = self.notify_hwnd.load(Ordering::Relaxed);
        if h != 0 {
            unsafe {
                let _ = PostMessageW(Some(HWND(h as *mut _)), WM_APP_ENGINE, WPARAM(0), LPARAM(0));
            }
        }
    }
}

pub fn spawn(shared: Arc<Shared>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new().name("tweenframe-engine".into()).spawn(move || run(shared)).unwrap()
}

enum End {
    Quit,
    Again,
}

fn run(shared: Arc<Shared>) {
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
    }
    while !shared.quit.load(Ordering::Relaxed) {
        if shared.multiplier.load(Ordering::Relaxed) <= 1 {
            shared.set_status(Status::Off);
            shared.wait_wake(500);
            continue;
        }
        let matcher = shared.matcher.lock().unwrap().clone();
        let Some(hwnd) = matcher.find() else {
            shared.set_status(Status::Waiting);
            shared.wait_wake(700);
            continue;
        };
        match session(&shared, hwnd) {
            Ok(End::Quit) => break,
            Ok(End::Again) => {}
            Err(e) => {
                crate::log!("session error: {e:?}");
                shared.set_status(Status::Error);
                shared.wait_wake(1500);
            }
        }
    }
    shared.set_status(Status::Off);
}

/// Running estimates of the game's frame timing (all in 100 ns units).
struct Pacer {
    interval: f64,
    latency: f64,
    jitter: f64,
    last_t: Option<i64>,
    outliers: u32,
    frames: u64,
}

impl Pacer {
    fn new() -> Pacer {
        Pacer { interval: 100_000.0, latency: 0.0, jitter: 0.0, last_t: None, outliers: 0, frames: 0 }
    }

    fn on_frame(&mut self, t: i64, arrival: i64) {
        if let Some(last) = self.last_t {
            let dt = (t - last) as f64;
            if dt > 0.0 {
                if self.frames < 3 {
                    self.interval = dt;
                } else if dt < self.interval * 2.5 && dt > self.interval * 0.4 {
                    self.interval += (dt - self.interval) * 0.1;
                    self.jitter += ((dt - self.interval).abs() - self.jitter) * 0.1;
                    self.outliers = 0;
                } else {
                    // A hitch, or the frame rate really changed: adopt after 3 in a row.
                    self.outliers += 1;
                    if self.outliers >= 3 {
                        self.interval = dt;
                        self.outliers = 0;
                    }
                }
            }
        }
        // May be negative: WGC stamps frames with DWM's composition target time,
        // which can lie slightly in the future. The delay formula stays correct
        // either way (tau lands right before the previous frame at arrival).
        let lat = ((arrival - t) as f64).clamp(-50_000.0, 500_000.0);
        self.latency = if self.frames < 8 { lat } else { self.latency + (lat - self.latency) * 0.1 };
        self.interval = self.interval.clamp(20_000.0, 1_000_000.0); // 2 ms .. 100 ms
        self.last_t = Some(t);
        self.frames += 1;
    }

    /// How far behind "now" the displayed scene time is.
    fn delay(&self) -> i64 {
        let margin = (2.0 * self.jitter).clamp(5_000.0, 0.35 * self.interval);
        (self.interval + self.latency + margin + 3_000.0) as i64
    }
}

fn session(shared: &Shared, hwnd: HWND) -> Result<End> {
    let Some(geo) = target::geometry(hwnd) else { return Ok(End::Again) };
    if geo.width() < 64 || geo.height() < 64 || unsafe { IsIconic(hwnd).as_bool() } {
        shared.set_status(Status::Paused);
        shared.wait_wake(500);
        return Ok(End::Again);
    }
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    let gpu = Gpu::new(Some(monitor))?;
    let refresh = refresh_hz(monitor);
    shared.refresh_hz.store(refresh as u32, Ordering::Relaxed);
    crate::log!(
        "session: {} | game {}x{} at ({}, {}) crop ({}, {}) | monitor {} Hz",
        gpu.adapter_name,
        geo.width(),
        geo.height(),
        geo.client.left,
        geo.client.top,
        geo.crop_x,
        geo.crop_y,
        refresh
    );
    let mut capture = Capture::new(&gpu, hwnd)?;
    let mut fg = FrameGen::new(&gpu, geo.width(), geo.height(), 720)?;
    if shared.debug_tint.load(Ordering::Relaxed) {
        fg.params.debug_tint = 1.0;
    }
    let mut overlay = Overlay::new(&gpu, geo.client)?;
    let timer = unsafe {
        CreateWaitableTimerExW(None, PCWSTR::null(), CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS.0)?
    };
    let result = session_loop(shared, hwnd, geo, refresh, &mut capture, &mut fg, &mut overlay, timer);
    unsafe {
        let _ = CloseHandle(timer);
    }
    overlay.hide();
    result
}

#[allow(clippy::too_many_arguments)]
fn session_loop(
    shared: &Shared,
    hwnd: HWND,
    geo: target::Geometry,
    refresh: f64,
    capture: &mut Capture,
    fg: &mut FrameGen,
    overlay: &mut Overlay,
    timer: HANDLE,
) -> Result<End> {
    let mut pacer = Pacer::new();
    let mut not_needed = false;
    let mut have_slot = false;
    let mut next_due: i64 = 0;
    let mut last_shown: Option<Shown> = None;
    let mut last_arrival: i64 = 0;
    let mut last_check: i64 = 0;
    let mut stat_start = now_100ns();
    let mut stat_presents = 0u32;
    let mut stat_generated = 0u32;
    let mut stat_frames = 0u32;
    let mut stat_log_at = now_100ns() + 50_000_000;
    let crop_box = D3D11_BOX {
        left: geo.crop_x,
        top: geo.crop_y,
        front: 0,
        right: geo.crop_x + geo.width(),
        bottom: geo.crop_y + geo.height(),
        back: 1,
    };

    loop {
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if shared.quit.load(Ordering::Relaxed) {
            return Ok(End::Quit);
        }
        let mult = shared.multiplier.load(Ordering::Relaxed);
        if mult <= 1 {
            return Ok(End::Again);
        }

        let now = now_100ns();
        if now - last_check > 1_000_000 {
            last_check = now;
            unsafe {
                if !IsWindow(Some(hwnd)).as_bool() {
                    crate::log!("game window closed");
                    return Ok(End::Again);
                }
            }
            match target::geometry(hwnd) {
                Some(g) if g == geo => {}
                _ => {
                    crate::log!("game window moved / resized, restarting session");
                    return Ok(End::Again);
                }
            }
            let foreground =
                shared.ignore_foreground.load(Ordering::Relaxed) || unsafe { GetForegroundWindow() } == hwnd;
            let flowing = now - last_arrival < 10_000_000;
            let playing = foreground && flowing && unsafe { !IsIconic(hwnd).as_bool() };

            if playing && !not_needed {
                if !overlay.visible {
                    overlay.show();
                    last_shown = None;
                }
                overlay.keep_on_top();
                shared.set_status(Status::Active);
            } else {
                overlay.hide();
                shared.set_status(if playing { Status::NotNeeded } else { Status::Paused });
            }

            let elapsed = (now - stat_start) as f64 / 1e7;
            if elapsed >= 1.0 {
                // Real frames counted over the whole second. (Per-frame intervals
                // are quantised to the display refresh by DWM, too noisy here.)
                let base = stat_frames as f64 / elapsed;
                let out = stat_presents as f64 / elapsed;

                // Game already at (almost) the refresh rate: generated frames could
                // not be displayed, they would only add latency. Step aside.
                if pacer.frames > 30 {
                    if !not_needed && base > refresh * BYPASS_ENTER {
                        not_needed = true;
                        crate::log!("game at {base:.0} fps on a {refresh} Hz monitor: frame generation not needed");
                    } else if not_needed && base < refresh * BYPASS_LEAVE {
                        not_needed = false;
                        crate::log!("game at {base:.0} fps: frame generation back on");
                    }
                }
                shared.base_fps_x10.store((base * 10.0) as u32, Ordering::Relaxed);
                shared.out_fps_x10.store((out * 10.0) as u32, Ordering::Relaxed);
                shared.notify();
                if now > stat_log_at {
                    crate::log!(
                        "stats: game {:.1} fps, output {:.1} fps ({} generated/s), delay {:.1} ms, capture latency {:.1} ms, jitter {:.2} ms",
                        base,
                        out,
                        (stat_generated as f64 / elapsed) as u32,
                        pacer.delay() as f64 / 1e4,
                        pacer.latency / 1e4,
                        pacer.jitter / 1e4
                    );
                    stat_log_at = now + 100_000_000;
                }
                stat_start = now;
                stat_presents = 0;
                stat_generated = 0;
                stat_frames = 0;
            }
        }

        let handles = [capture.event, overlay.waitable, timer, shared.wake_handle()];
        let r = unsafe { WaitForMultipleObjects(&handles, false, 50) };
        if r.0 == WAIT_OBJECT_0.0 + 1 {
            have_slot = true;
        }

        // Ingest every new game frame.
        while let Some(frame) = capture.try_next()? {
            let arrival = now_100ns();
            if frame.width < crop_box.right || frame.height < crop_box.bottom {
                continue; // window is being resized; the geometry check restarts us
            }
            if arrival - frame.time > 2_000_000 {
                continue; // stale frame queued before the session started
            }
            if overlay.visible {
                fg.push_frame(&frame.texture, Some(&crop_box), frame.time)?;
            } else {
                // Nothing on screen: keep the newest image, skip the analysis.
                fg.push_frame_copy_only(&frame.texture, Some(&crop_box), frame.time)?;
            }
            pacer.on_frame(frame.time, arrival);
            last_arrival = arrival;
            stat_frames += 1;
        }

        if !overlay.visible || !have_slot || fg.frame_count() == 0 {
            continue;
        }
        let now = now_100ns();
        let out_interval = (pacer.interval / mult as f64) as i64;
        // Present slightly early is fine: vsync snaps it to the right refresh.
        let early = out_interval / 8;
        if now + early < next_due {
            // Not due yet (output below refresh rate): sleep precisely until it is.
            let due = -(next_due - early - now).max(1);
            unsafe {
                let _ = SetWaitableTimer(timer, &due, 0, None, None, false);
            }
            continue;
        }
        let tau = if pacer.frames < 3 { i64::MAX } else { now - pacer.delay() };
        let max_gap = (pacer.interval * 2.5) as i64;
        if let Some(shown) = fg.render(tau, max_gap, &overlay.rtv, &overlay.backbuffer)? {
            if Some(shown) != last_shown {
                overlay.present()?;
                have_slot = false;
                stat_presents += 1;
                if matches!(shown, Shown::Generated { .. }) {
                    stat_generated += 1;
                }
                last_shown = Some(shown);
            }
        }
        next_due += out_interval;
        if next_due < now - out_interval {
            next_due = now + out_interval; // fell behind (e.g. overlay was hidden)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Pacer;

    /// Feeds `n` frames at `fps` with a fixed capture latency; returns the last timestamp.
    fn feed(p: &mut Pacer, start: i64, fps: f64, n: usize, latency: i64) -> i64 {
        let dt = (1e7 / fps) as i64;
        let mut t = start;
        for _ in 0..n {
            t += dt;
            p.on_frame(t, t + latency);
        }
        t
    }

    #[test]
    fn steady_100fps() {
        let mut p = Pacer::new();
        feed(&mut p, 0, 100.0, 200, 20_000);
        assert!((p.interval - 100_000.0).abs() < 500.0, "interval {}", p.interval);
        assert!((p.latency - 20_000.0).abs() < 500.0, "latency {}", p.latency);
        // delay = one frame + latency + small margin
        let d = p.delay();
        assert!(d > 120_000 && d < 130_000, "delay {d}");
    }

    #[test]
    fn negative_latency_lowers_delay() {
        let mut p = Pacer::new();
        feed(&mut p, 0, 100.0, 200, -15_000);
        assert!(p.delay() < 100_000, "delay {}", p.delay());
    }

    #[test]
    fn adapts_to_new_frame_rate_and_ignores_single_hitch() {
        let mut p = Pacer::new();
        let t = feed(&mut p, 0, 100.0, 100, 0);
        // one 50 ms hitch must not change the estimate
        p.on_frame(t + 500_000, t + 500_000);
        assert!((p.interval - 100_000.0).abs() < 5_000.0, "interval after hitch {}", p.interval);
        // a real drop to 30 fps is adopted
        feed(&mut p, t + 500_000, 30.0, 50, 0);
        assert!((p.interval - 333_333.0).abs() < 20_000.0, "interval {}", p.interval);
    }
}
