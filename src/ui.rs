//! The small settings window (status, 1x / 2x / 3x, Apply) and the tray icon.
//! Native Win32 + Direct2D: tiny, instant, no GPU context of its own to speak of.

use crate::config::Config;
use crate::engine::{Shared, Status, WM_APP_ENGINE};
use crate::icon;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use windows::core::{w, Result, PCSTR, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Globalization::GetUserDefaultUILanguage;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow, GetSystemMetricsForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

const CLASS: PCWSTR = w!("TweenFrameMain");
const WM_APP_TRAY: u32 = WM_APP + 1;
/// Sent by a second instance to bring the window up.
pub const WM_APP_SHOW: u32 = WM_APP + 2;
/// Sent by `tweenframe.exe --quit` to close the running instance.
pub const WM_APP_QUIT: u32 = WM_APP + 3;
const TRAY_ID: u32 = 1;
const WM_MOUSELEAVE: u32 = 0x02A3;
const NIN_KEYSELECT: u32 = 0x0401;
const MENU_OPEN: usize = 100;
const MENU_QUIT: usize = 101;

// Layout in DIPs.
const W: f32 = 340.0;
const H: f32 = 272.0;
const PAD: f32 = 20.0;

fn rgb(hex: u32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: ((hex >> 16) & 0xff) as f32 / 255.0,
        g: ((hex >> 8) & 0xff) as f32 / 255.0,
        b: (hex & 0xff) as f32 / 255.0,
        a: 1.0,
    }
}

mod color {
    pub const BG: u32 = 0x0F1115;
    pub const CARD: u32 = 0x181B21;
    pub const BORDER: u32 = 0x262A33;
    pub const TEXT: u32 = 0xF2F4F8;
    pub const MUTED: u32 = 0x8B93A1;
    pub const ACCENT: u32 = 0x4F7CFF;
    pub const ACCENT_HOVER: u32 = 0x6A90FF;
    pub const ACCENT_DOWN: u32 = 0x3F68E0;
    pub const SEG_HOVER: u32 = 0x22262E;
    pub const GREEN: u32 = 0x22C55E;
    pub const AMBER: u32 = 0xF5A524;
    pub const BLUE: u32 = 0x60A5FA;
    pub const GREY: u32 = 0x6B7280;
    pub const RED: u32 = 0xEF4444;
    pub const TEAL: u32 = 0x2DD4BF;
}

struct Strings {
    status: [&'static str; 6],
    /// Format: game fps, recommended cap, multiplier.
    sub_not_needed: &'static str,
    sub_off: &'static str,
    sub_waiting: &'static str,
    sub_active: &'static str,
    sub_paused: &'static str,
    sub_error: &'static str,
    label: &'static str,
    hints: [&'static str; 3],
    pending: &'static str,
    apply: &'static str,
    open: &'static str,
    quit: &'static str,
    hint_title: &'static str,
    hint_body: &'static str,
}

const DE: Strings = Strings {
    status: ["Nicht aktiv", "Wartet auf FiveM", "Aktiv", "Pausiert", "Fehler", "Nicht nötig"],
    sub_not_needed: "FiveM hat schon {fps} FPS · Limit {cap} für {m}x",
    sub_off: "Wähle 2x oder 3x und klicke Anwenden",
    sub_waiting: "Starte FiveM im randlosen Fenstermodus",
    sub_active: "FiveM",
    sub_paused: "FiveM ist nicht im Vordergrund",
    sub_error: "Neuer Versuch läuft · Details in log.txt",
    label: "FRAME GENERATION",
    hints: ["Aus – keine Zwischenbilder", "1 Zwischenbild pro Frame", "2 Zwischenbilder pro Frame"],
    pending: "nicht angewendet",
    apply: "Anwenden",
    open: "Öffnen",
    quit: "Beenden",
    hint_title: "TweenFrame läuft im Hintergrund",
    hint_body: "Du findest es unten rechts in der Taskleiste. Klick auf das Symbol öffnet die Einstellungen.",
};

const EN: Strings = Strings {
    status: ["Not active", "Waiting for FiveM", "Active", "Paused", "Error", "Not needed"],
    sub_not_needed: "FiveM already runs {fps} FPS · cap at {cap} for {m}x",
    sub_off: "Pick 2x or 3x and press Apply",
    sub_waiting: "Start FiveM in borderless window mode",
    sub_active: "FiveM",
    sub_paused: "FiveM is not in the foreground",
    sub_error: "Retrying · details in log.txt",
    label: "FRAME GENERATION",
    hints: ["Off – no generated frames", "1 extra frame per real frame", "2 extra frames per real frame"],
    pending: "not applied yet",
    apply: "Apply",
    open: "Open",
    quit: "Quit",
    hint_title: "TweenFrame keeps running",
    hint_body: "You'll find it in the taskbar corner. Click the icon to open the settings.",
};

#[derive(Clone, Copy, PartialEq, Debug)]
enum Hit {
    None,
    Seg(u32),
    Apply,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn rect(l: f32, t: f32, r: f32, b: f32) -> D2D_RECT_F {
    D2D_RECT_F { left: l, top: t, right: r, bottom: b }
}

fn seg_rect(i: u32) -> D2D_RECT_F {
    let sw = (W - 2.0 * PAD - 8.0) / 3.0;
    let x0 = PAD + 4.0 + i as f32 * sw;
    rect(x0, 128.0, x0 + sw, 164.0)
}

const APPLY_RECT: D2D_RECT_F = D2D_RECT_F { left: PAD, top: 206.0, right: W - PAD, bottom: 250.0 };

fn contains(r: &D2D_RECT_F, x: f32, y: f32) -> bool {
    x >= r.left && x <= r.right && y >= r.top && y <= r.bottom
}

struct Fonts {
    title: IDWriteTextFormat,
    body: IDWriteTextFormat,
    label: IDWriteTextFormat,
    seg: IDWriteTextFormat,
    button: IDWriteTextFormat,
}

struct Ui {
    hwnd: HWND,
    shared: Arc<Shared>,
    cfg: Config,
    s: &'static Strings,
    selected: u32,
    hover: Hit,
    pressed: Hit,
    tracking: bool,
    d2d: ID2D1Factory,
    fonts: Fonts,
    rt: Option<ID2D1HwndRenderTarget>,
    brush: Option<ID2D1SolidColorBrush>,
    dpi: u32,
    icon_idle: HICON,
    icon_active: HICON,
    icon_big: HICON,
    tray_active: Option<bool>,
    taskbar_created: u32,
    pin_attempts: u32,
}

const TIMER_PIN: usize = 1;

impl Ui {
    fn status(&self) -> Status {
        self.shared.status()
    }

    fn create_fonts(dw: &IDWriteFactory) -> Result<Fonts> {
        let mk = |size: f32, weight: DWRITE_FONT_WEIGHT, center: bool| -> Result<IDWriteTextFormat> {
            unsafe {
                let f = dw.CreateTextFormat(
                    w!("Segoe UI Variable Text"),
                    None,
                    weight,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size,
                    w!(""),
                )?;
                f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
                if center {
                    f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
                }
                f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
                Ok(f)
            }
        };
        Ok(Fonts {
            title: mk(15.0, DWRITE_FONT_WEIGHT_SEMI_BOLD, false)?,
            body: mk(12.5, DWRITE_FONT_WEIGHT_NORMAL, false)?,
            label: mk(11.0, DWRITE_FONT_WEIGHT_SEMI_BOLD, false)?,
            seg: mk(15.0, DWRITE_FONT_WEIGHT_SEMI_BOLD, true)?,
            button: mk(14.0, DWRITE_FONT_WEIGHT_SEMI_BOLD, true)?,
        })
    }

    fn ensure_target(&mut self) -> Result<()> {
        if self.rt.is_some() {
            return Ok(());
        }
        unsafe {
            let mut rc = RECT::default();
            GetClientRect(self.hwnd, &mut rc)?;
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 0.0,
                dpiY: 0.0,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            let hprops = D2D1_HWND_RENDER_TARGET_PROPERTIES {
                hwnd: self.hwnd,
                pixelSize: D2D_SIZE_U { width: rc.right as u32, height: rc.bottom as u32 },
                presentOptions: D2D1_PRESENT_OPTIONS_NONE,
            };
            let rt = self.d2d.CreateHwndRenderTarget(&props, &hprops)?;
            rt.SetDpi(self.dpi as f32, self.dpi as f32);
            rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            self.brush = Some(rt.CreateSolidColorBrush(&rgb(color::TEXT), None)?);
            self.rt = Some(rt);
        }
        Ok(())
    }

    fn paint(&mut self) -> Result<()> {
        self.ensure_target()?;
        let rt = self.rt.clone().unwrap();
        let br = self.brush.clone().unwrap();
        let status = self.status();
        let applied = self.shared.multiplier.load(Ordering::Relaxed);
        let s = self.s;
        unsafe {
            rt.BeginDraw();
            rt.Clear(Some(&rgb(color::BG)));

            let fill = |r: D2D_RECT_F, rad: f32, c: u32| {
                br.SetColor(&rgb(c));
                rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: r, radiusX: rad, radiusY: rad }, &br);
            };
            let stroke = |r: D2D_RECT_F, rad: f32, c: u32| {
                br.SetColor(&rgb(c));
                rt.DrawRoundedRectangle(&D2D1_ROUNDED_RECT { rect: r, radiusX: rad, radiusY: rad }, &br, 1.0, None);
            };
            let text = |t: &str, f: &IDWriteTextFormat, r: D2D_RECT_F, c: u32| {
                br.SetColor(&rgb(c));
                rt.DrawText(&wide(t), f, &r, &br, D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
            };

            // Status card
            let card = rect(PAD, 20.0, W - PAD, 84.0);
            fill(card, 12.0, color::CARD);
            stroke(rect(card.left + 0.5, card.top + 0.5, card.right - 0.5, card.bottom - 0.5), 12.0, color::BORDER);
            let dot = match status {
                Status::Off => color::GREY,
                Status::Waiting => color::AMBER,
                Status::Active => color::GREEN,
                Status::Paused => color::BLUE,
                Status::Error => color::RED,
                Status::NotNeeded => color::TEAL,
            };
            let (cx, cy) = (PAD + 22.0, 52.0);
            if status == Status::Active {
                br.SetOpacity(0.22);
                fill(rect(cx - 9.0, cy - 9.0, cx + 9.0, cy + 9.0), 9.0, dot);
                br.SetOpacity(1.0);
            }
            fill(rect(cx - 5.0, cy - 5.0, cx + 5.0, cy + 5.0), 5.0, dot);
            let title = s.status[status as usize];
            text(title, &self.fonts.title, rect(PAD + 42.0, 28.0, W - PAD - 14.0, 52.0), color::TEXT);
            let sub = match status {
                Status::Off => s.sub_off.to_string(),
                Status::Waiting => s.sub_waiting.to_string(),
                Status::Active => {
                    let base = self.shared.base_fps_x10.load(Ordering::Relaxed) as f32 / 10.0;
                    let out = self.shared.out_fps_x10.load(Ordering::Relaxed) as f32 / 10.0;
                    if base > 0.0 {
                        format!("{} · {applied}x · {:.0} → {:.0} FPS", s.sub_active, base, out)
                    } else {
                        format!("{} · {applied}x", s.sub_active)
                    }
                }
                Status::Paused => s.sub_paused.to_string(),
                Status::Error => s.sub_error.to_string(),
                Status::NotNeeded => {
                    let base = self.shared.base_fps_x10.load(Ordering::Relaxed) / 10;
                    let hz = self.shared.refresh_hz.load(Ordering::Relaxed).max(60);
                    let m = applied.max(2);
                    s.sub_not_needed
                        .replace("{fps}", &base.to_string())
                        .replace("{cap}", &(hz / m).to_string())
                        .replace("{m}", &m.to_string())
                }
            };
            text(&sub, &self.fonts.body, rect(PAD + 42.0, 52.0, W - PAD - 14.0, 74.0), color::MUTED);

            // Multiplier
            text(s.label, &self.fonts.label, rect(PAD + 2.0, 100.0, W - PAD, 120.0), color::MUTED);
            let segbox = rect(PAD, 124.0, W - PAD, 168.0);
            fill(segbox, 11.0, color::CARD);
            stroke(
                rect(segbox.left + 0.5, segbox.top + 0.5, segbox.right - 0.5, segbox.bottom - 0.5),
                11.0,
                color::BORDER,
            );
            for i in 0..3u32 {
                let m = i + 1;
                let r = seg_rect(i);
                let sel = self.selected == m;
                if sel {
                    fill(r, 8.0, color::ACCENT);
                } else if self.hover == Hit::Seg(i) {
                    fill(r, 8.0, color::SEG_HOVER);
                }
                let c = if sel || self.hover == Hit::Seg(i) { color::TEXT } else { color::MUTED };
                text(&format!("{m}x"), &self.fonts.seg, r, c);
            }
            let mut hint = s.hints[(self.selected - 1) as usize].to_string();
            if self.selected != applied {
                hint = format!("{hint} · {}", s.pending);
            }
            text(&hint, &self.fonts.body, rect(PAD + 2.0, 174.0, W - PAD, 196.0), color::MUTED);

            // Apply
            let bc = match (self.pressed == Hit::Apply && self.hover == Hit::Apply, self.hover == Hit::Apply) {
                (true, _) => color::ACCENT_DOWN,
                (false, true) => color::ACCENT_HOVER,
                _ => color::ACCENT,
            };
            fill(APPLY_RECT, 10.0, bc);
            text(s.apply, &self.fonts.button, APPLY_RECT, 0xFFFFFF);

            if let Err(e) = rt.EndDraw(None, None) {
                if e.code() == D2DERR_RECREATE_TARGET {
                    self.rt = None;
                    self.brush = None;
                }
            }
        }
        Ok(())
    }

    fn hit(&self, lp: LPARAM) -> Hit {
        let x = (lp.0 & 0xffff) as i16 as f32 * 96.0 / self.dpi as f32;
        let y = ((lp.0 >> 16) & 0xffff) as i16 as f32 * 96.0 / self.dpi as f32;
        for i in 0..3 {
            if contains(&seg_rect(i), x, y) {
                return Hit::Seg(i);
            }
        }
        if contains(&APPLY_RECT, x, y) {
            return Hit::Apply;
        }
        Hit::None
    }

    fn invalidate(&self) {
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    fn select(&mut self, m: u32) {
        self.selected = m.clamp(1, 3);
        self.invalidate();
    }

    fn apply(&mut self) {
        self.cfg.multiplier = self.selected;
        self.cfg.save();
        self.shared.multiplier.store(self.selected, Ordering::Relaxed);
        self.shared.wake();
        crate::log!("applied {}x", self.selected);
        self.update_tray();
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
        if !self.cfg.tray_hint_shown {
            self.cfg.tray_hint_shown = true;
            self.cfg.save();
            self.tray_balloon(self.s.hint_title, self.s.hint_body);
        }
    }

    /// One-time Windows notification anchored to the tray icon.
    fn tray_balloon(&self, title: &str, body: &str) {
        let mut nid = self.tray_data();
        nid.uFlags = NIF_INFO;
        nid.dwInfoFlags = NIIF_USER | NIIF_LARGE_ICON;
        nid.hBalloonIcon = self.icon_big;
        for (i, c) in title.encode_utf16().take(63).enumerate() {
            nid.szInfoTitle[i] = c;
        }
        for (i, c) in body.encode_utf16().take(255).enumerate() {
            nid.szInfo[i] = c;
        }
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }

    fn show(&mut self) {
        self.selected = self.shared.multiplier.load(Ordering::Relaxed).max(1);
        if self.selected == 1 {
            self.selected = self.cfg.multiplier.clamp(1, 3);
        }
        unsafe {
            if IsIconic(self.hwnd).as_bool() {
                let _ = ShowWindow(self.hwnd, SW_RESTORE);
            } else {
                let _ = ShowWindow(self.hwnd, SW_SHOW);
            }
            let _ = SetForegroundWindow(self.hwnd);
        }
        self.invalidate();
    }

    fn tray_data(&self) -> NOTIFYICONDATAW {
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: TRAY_ID,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP,
            uCallbackMessage: WM_APP_TRAY,
            hIcon: if self.status() == Status::Active { self.icon_active } else { self.icon_idle },
            ..Default::default()
        };
        let status = self.s.status[self.status() as usize];
        let mult = self.shared.multiplier.load(Ordering::Relaxed);
        let tip =
            if mult > 1 { format!("TweenFrame · {status} · {mult}x") } else { format!("TweenFrame · {status}") };
        for (i, c) in tip.encode_utf16().take(127).enumerate() {
            nid.szTip[i] = c;
        }
        nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        nid
    }

    fn add_tray(&mut self) {
        let nid = self.tray_data();
        unsafe {
            let _ = Shell_NotifyIconW(NIM_ADD, &nid);
            let _ = Shell_NotifyIconW(NIM_SETVERSION, &nid);
        }
        self.tray_active = Some(self.status() == Status::Active);
    }

    fn update_tray(&mut self) {
        let nid = self.tray_data();
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
        self.tray_active = Some(self.status() == Status::Active);
    }

    fn remove_tray(&self) {
        let nid = self.tray_data();
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
        }
    }

    fn tray_menu(&mut self, x: i32, y: i32) {
        unsafe {
            let Ok(menu) = CreatePopupMenu() else { return };
            let ws = wide(&format!("{}\0", self.s.status[self.status() as usize]));
            let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, PCWSTR(ws.as_ptr()));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            let wo = wide(&format!("{}\0", self.s.open));
            let wq = wide(&format!("{}\0", self.s.quit));
            let _ = AppendMenuW(menu, MF_STRING, MENU_OPEN, PCWSTR(wo.as_ptr()));
            let _ = AppendMenuW(menu, MF_STRING, MENU_QUIT, PCWSTR(wq.as_ptr()));
            let _ = SetForegroundWindow(self.hwnd);
            let cmd = TrackPopupMenu(
                menu,
                TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY | TPM_BOTTOMALIGN,
                x,
                y,
                None,
                self.hwnd,
                None,
            );
            let _ = PostMessageW(Some(self.hwnd), WM_NULL, WPARAM(0), LPARAM(0));
            let _ = DestroyMenu(menu);
            match cmd.0 as usize {
                MENU_OPEN => self.show(),
                MENU_QUIT => self.quit(),
                _ => {}
            }
        }
    }

    fn quit(&mut self) {
        self.shared.quit.store(true, Ordering::Relaxed);
        self.shared.wake();
        self.remove_tray();
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }

    fn handle(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_PAINT => {
                unsafe {
                    let mut ps = PAINTSTRUCT::default();
                    BeginPaint(self.hwnd, &mut ps);
                    let _ = self.paint();
                    let _ = EndPaint(self.hwnd, &ps);
                }
                Some(LRESULT(0))
            }
            WM_ERASEBKGND => Some(LRESULT(1)),
            WM_SIZE => {
                if let Some(rt) = &self.rt {
                    let (w, h) = ((lp.0 & 0xffff) as u32, ((lp.0 >> 16) & 0xffff) as u32);
                    unsafe {
                        let _ = rt.Resize(&D2D_SIZE_U { width: w, height: h });
                    }
                }
                None
            }
            WM_DPICHANGED => {
                self.dpi = ((wp.0 >> 16) & 0xffff) as u32;
                let r = unsafe { &*(lp.0 as *const RECT) };
                unsafe {
                    let _ = SetWindowPos(
                        self.hwnd,
                        None,
                        r.left,
                        r.top,
                        r.right - r.left,
                        r.bottom - r.top,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
                self.rt = None;
                self.brush = None;
                self.invalidate();
                Some(LRESULT(0))
            }
            WM_MOUSEMOVE => {
                if !self.tracking {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: self.hwnd,
                        dwHoverTime: 0,
                    };
                    unsafe {
                        let _ = TrackMouseEvent(&mut tme);
                    }
                    self.tracking = true;
                }
                let h = self.hit(lp);
                if h != self.hover {
                    self.hover = h;
                    self.invalidate();
                }
                Some(LRESULT(0))
            }
            WM_MOUSELEAVE => {
                self.tracking = false;
                self.hover = Hit::None;
                self.invalidate();
                Some(LRESULT(0))
            }
            WM_SETCURSOR => {
                if (lp.0 & 0xffff) as u32 == HTCLIENT && self.hover != Hit::None {
                    unsafe {
                        if let Ok(c) = LoadCursorW(None, IDC_HAND) {
                            SetCursor(Some(c));
                        }
                    }
                    return Some(LRESULT(1));
                }
                None
            }
            WM_LBUTTONDOWN => {
                self.pressed = self.hit(lp);
                if let Hit::Seg(i) = self.pressed {
                    self.select(i + 1);
                }
                unsafe {
                    SetCapture(self.hwnd);
                }
                self.invalidate();
                Some(LRESULT(0))
            }
            WM_LBUTTONUP => {
                unsafe {
                    let _ = ReleaseCapture();
                }
                let h = self.hit(lp);
                let was = self.pressed;
                self.pressed = Hit::None;
                self.invalidate();
                if was == Hit::Apply && h == Hit::Apply {
                    self.apply();
                }
                Some(LRESULT(0))
            }
            WM_KEYDOWN => {
                let vk = wp.0 as u16;
                match VIRTUAL_KEY(vk) {
                    VK_1 | VK_NUMPAD1 => self.select(1),
                    VK_2 | VK_NUMPAD2 => self.select(2),
                    VK_3 | VK_NUMPAD3 => self.select(3),
                    VK_LEFT => self.select(self.selected.saturating_sub(1).max(1)),
                    VK_RIGHT => self.select(self.selected + 1),
                    VK_RETURN | VK_SPACE => self.apply(),
                    VK_ESCAPE => unsafe {
                        let _ = ShowWindow(self.hwnd, SW_HIDE);
                    },
                    _ => {}
                }
                Some(LRESULT(0))
            }
            WM_CLOSE => {
                // Closing only hides the window; the app keeps running in the tray.
                unsafe {
                    let _ = ShowWindow(self.hwnd, SW_HIDE);
                }
                Some(LRESULT(0))
            }
            WM_APP_ENGINE => {
                let active = self.status() == Status::Active;
                if self.tray_active != Some(active) {
                    self.update_tray();
                }
                if unsafe { IsWindowVisible(self.hwnd).as_bool() } {
                    self.invalidate();
                }
                Some(LRESULT(0))
            }
            WM_APP_SHOW => {
                self.show();
                Some(LRESULT(0))
            }
            WM_APP_QUIT => {
                self.quit();
                Some(LRESULT(0))
            }
            WM_TIMER if wp.0 == TIMER_PIN => {
                // Explorer registers the icon asynchronously, so retry for a few seconds.
                self.pin_attempts += 1;
                let done = match crate::tray_pin::pin_own_icon() {
                    crate::tray_pin::PinResult::Pinned => {
                        // Re-adding makes Explorer pick up the new setting immediately.
                        self.remove_tray();
                        self.add_tray();
                        true
                    }
                    crate::tray_pin::PinResult::AlreadyDecided => true,
                    crate::tray_pin::PinResult::NotFound => self.pin_attempts >= 10,
                };
                if done {
                    unsafe {
                        let _ = KillTimer(Some(self.hwnd), TIMER_PIN);
                    }
                }
                Some(LRESULT(0))
            }
            WM_APP_TRAY => {
                let event = (lp.0 & 0xffff) as u32;
                let x = (wp.0 & 0xffff) as i16 as i32;
                let y = ((wp.0 >> 16) & 0xffff) as i16 as i32;
                match event {
                    NIN_SELECT | NIN_KEYSELECT | WM_LBUTTONUP => self.show(),
                    WM_CONTEXTMENU | WM_RBUTTONUP => self.tray_menu(x, y),
                    _ => {}
                }
                Some(LRESULT(0))
            }
            WM_DESTROY => {
                unsafe { PostQuitMessage(0) };
                Some(LRESULT(0))
            }
            _ if msg == self.taskbar_created && msg != 0 => {
                self.add_tray();
                Some(LRESULT(0))
            }
            _ => None,
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Ui;
    if !ptr.is_null() {
        if let Some(r) = (*ptr).handle(msg, wp, lp) {
            return r;
        }
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

/// Opts the process into dark context menus (undocumented uxtheme ordinals,
/// used by many apps; silently does nothing if unavailable).
fn enable_dark_menus() {
    unsafe {
        if let Ok(ux) = LoadLibraryW(w!("uxtheme.dll")) {
            if let Some(f) = GetProcAddress(ux, PCSTR(135 as *const u8)) {
                let set_preferred_app_mode: extern "system" fn(i32) -> i32 = std::mem::transmute(f);
                set_preferred_app_mode(2); // ForceDark
            }
            if let Some(f) = GetProcAddress(ux, PCSTR(136 as *const u8)) {
                let flush: extern "system" fn() = std::mem::transmute(f);
                flush();
            }
        }
    }
}

fn set_dwm_int(hwnd: HWND, attr: i32, value: u32) {
    unsafe {
        let _ = DwmSetWindowAttribute(hwnd, DWMWINDOWATTRIBUTE(attr), &value as *const u32 as *const _, 4);
    }
}

fn colorref(hex: u32) -> u32 {
    ((hex & 0xff) << 16) | (hex & 0xff00) | ((hex >> 16) & 0xff)
}

pub fn run(cfg: Config, shared: Arc<Shared>, start_hidden: bool) -> Result<()> {
    unsafe {
        let hinst = GetModuleHandleW(None)?;
        let german = (GetUserDefaultUILanguage() & 0x3ff) == 0x07;
        let strings: &'static Strings = if german { &DE } else { &EN };
        enable_dark_menus();

        let icon_big = icon::create(48, false)?;
        let icon_small = icon::create(GetSystemMetricsForDpi(SM_CXSMICON, 96).max(16) as u32 * 2, false)?;
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hIcon: icon_big,
            hIconSm: icon_small,
            ..Default::default()
        };
        RegisterClassExW(&wc);

        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU;
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS,
            w!("TweenFrame"),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            100,
            100,
            None,
            None,
            Some(hinst.into()),
            None,
        )?;
        let dpi = GetDpiForWindow(hwnd).max(96);

        // Size for the client area, centred on the screen the window opened on.
        let mut rc =
            RECT { left: 0, top: 0, right: (W * dpi as f32 / 96.0) as i32, bottom: (H * dpi as f32 / 96.0) as i32 };
        let _ = AdjustWindowRectExForDpi(&mut rc, style, false, WINDOW_EX_STYLE(0), dpi);
        let (ww, wh) = (rc.right - rc.left, rc.bottom - rc.top);
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTOPRIMARY);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(mon, &mut mi);
        let wa = mi.rcWork;
        let x = wa.left + ((wa.right - wa.left) - ww) / 2;
        let y = wa.top + ((wa.bottom - wa.top) - wh) / 2;
        let _ = SetWindowPos(hwnd, None, x, y, ww, wh, SWP_NOZORDER | SWP_NOACTIVATE);

        // Dark, seamless Windows 11 chrome.
        set_dwm_int(hwnd, 20, 1); // DWMWA_USE_IMMERSIVE_DARK_MODE
        set_dwm_int(hwnd, 33, 2); // DWMWA_WINDOW_CORNER_PREFERENCE = round
        set_dwm_int(hwnd, 35, colorref(color::BG)); // DWMWA_CAPTION_COLOR
        set_dwm_int(hwnd, 34, colorref(color::BORDER)); // DWMWA_BORDER_COLOR
        set_dwm_int(hwnd, 36, colorref(color::TEXT)); // DWMWA_TEXT_COLOR

        let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let tray_size = (GetSystemMetricsForDpi(SM_CXSMICON, dpi).max(16)) as u32;
        let mut ui = Box::new(Ui {
            hwnd,
            shared: shared.clone(),
            selected: cfg.multiplier.clamp(1, 3),
            cfg,
            s: strings,
            hover: Hit::None,
            pressed: Hit::None,
            tracking: false,
            d2d,
            fonts: Ui::create_fonts(&dw)?,
            rt: None,
            brush: None,
            dpi,
            icon_idle: icon::create(tray_size, false)?,
            icon_active: icon::create(tray_size, true)?,
            icon_big,
            tray_active: None,
            taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
            pin_attempts: 0,
        });
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, &mut *ui as *mut Ui as isize);
        let _ = SendMessageW(hwnd, WM_SETICON, Some(WPARAM(ICON_BIG as usize)), Some(LPARAM(ui.icon_big.0 as isize)));
        shared.notify_hwnd.store(hwnd.0 as isize, Ordering::Relaxed);
        ui.add_tray();
        SetTimer(Some(hwnd), TIMER_PIN, 700, None);
        if !start_hidden {
            ui.show();
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        shared.notify_hwnd.store(0, Ordering::Relaxed);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        drop(ui);
    }
    Ok(())
}

/// If another instance runs, send it `msg` (WM_APP_SHOW / WM_APP_QUIT) and return true.
pub fn signal_existing_instance(msg: u32) -> bool {
    unsafe {
        if let Ok(h) = FindWindowW(CLASS, None) {
            if !h.is_invalid() {
                let _ = PostMessageW(Some(h), msg, WPARAM(0), LPARAM(0));
                return true;
            }
        }
    }
    false
}
