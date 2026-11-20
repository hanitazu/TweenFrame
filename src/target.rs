//! Finds the game window. FiveM / GTA V use the RAGE window class `grcWindow`;
//! as a fallback, any large window of a FiveM game process is accepted.

use windows::core::{BOOL, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::*;

/// RAGE engine window classes: GTA V / FiveM, RDR2 / RedM.
pub const DEFAULT_CLASSES: &[&str] = &["grcWindow", "sgaWindow"];

/// Game process names (lower case, substring match), e.g. `FiveM_b3258_GTAProcess.exe`.
const GAME_PROCESSES: &[&str] = &["gtaprocess", "gta5.exe", "rdr2.exe"];

#[derive(Clone, Debug, Default)]
pub struct Matcher {
    pub classes: Vec<String>,
    pub title_contains: Option<String>,
}

fn process_name(hwnd: HWND) -> Option<String> {
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return None;
        }
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 520];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        ok.ok()?;
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        path.rsplit('\\').next().map(|s| s.to_lowercase())
    }
}

impl Matcher {
    pub fn new(extra_classes: &[String], title_contains: Option<String>) -> Matcher {
        let mut classes: Vec<String> = DEFAULT_CLASSES.iter().map(|s| s.to_string()).collect();
        classes.extend(extra_classes.iter().cloned());
        Matcher { classes, title_contains }
    }

    fn matches(&self, hwnd: HWND) -> bool {
        unsafe {
            if !IsWindowVisible(hwnd).as_bool() {
                return false;
            }
            if let Some(t) = &self.title_contains {
                let mut buf = [0u16; 256];
                let n = GetWindowTextW(hwnd, &mut buf);
                let title = String::from_utf16_lossy(&buf[..n.max(0) as usize]);
                return title.to_lowercase().contains(&t.to_lowercase());
            }
            let mut buf = [0u16; 128];
            let n = GetClassNameW(hwnd, &mut buf);
            let class = String::from_utf16_lossy(&buf[..n.max(0) as usize]);
            if self.classes.iter().any(|c| c.eq_ignore_ascii_case(&class)) {
                return true;
            }
            // Fallback: a big top-level window owned by a game process.
            let mut rc = RECT::default();
            if GetClientRect(hwnd, &mut rc).is_err() || rc.right < 640 || rc.bottom < 360 {
                return false;
            }
            if GetWindow(hwnd, GW_OWNER).map(|o| !o.is_invalid()).unwrap_or(false) {
                return false;
            }
            match process_name(hwnd) {
                Some(p) => GAME_PROCESSES.iter().any(|g| p.contains(g)),
                None => false,
            }
        }
    }

    /// The first visible matching top-level window.
    pub fn find(&self) -> Option<HWND> {
        struct State<'a> {
            m: &'a Matcher,
            found: Option<HWND>,
        }
        unsafe extern "system" fn cb(hwnd: HWND, lp: LPARAM) -> BOOL {
            let st = &mut *(lp.0 as *mut State);
            if st.m.matches(hwnd) {
                st.found = Some(hwnd);
                return BOOL(0);
            }
            BOOL(1)
        }
        let mut st = State { m: self, found: None };
        unsafe {
            let _ = EnumWindows(Some(cb), LPARAM(&mut st as *mut State as isize));
        }
        st.found
    }
}

/// Where the game's pixels are on screen and inside the captured surface.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Geometry {
    /// Client area in screen coordinates (physical pixels).
    pub client: RECT,
    /// Offset of the client area inside the WGC frame (window visual bounds).
    pub crop_x: u32,
    pub crop_y: u32,
}

impl Geometry {
    pub fn width(&self) -> u32 {
        (self.client.right - self.client.left).max(0) as u32
    }
    pub fn height(&self) -> u32 {
        (self.client.bottom - self.client.top).max(0) as u32
    }
}

pub fn geometry(hwnd: HWND) -> Option<Geometry> {
    unsafe {
        let mut rc = RECT::default();
        GetClientRect(hwnd, &mut rc).ok()?;
        let mut tl = POINT { x: 0, y: 0 };
        if !ClientToScreen(hwnd, &mut tl).as_bool() {
            return None;
        }
        let client = RECT { left: tl.x, top: tl.y, right: tl.x + rc.right, bottom: tl.y + rc.bottom };
        let mut frame = RECT::default();
        let ok = DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut frame as *mut RECT as *mut _,
            std::mem::size_of::<RECT>() as u32,
        );
        if ok.is_err() {
            GetWindowRect(hwnd, &mut frame).ok()?;
        }
        Some(Geometry {
            client,
            crop_x: (client.left - frame.left).max(0) as u32,
            crop_y: (client.top - frame.top).max(0) as u32,
        })
    }
}
