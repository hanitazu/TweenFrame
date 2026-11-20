//! Windows 11 puts new tray icons into the "^" overflow. The visibility switch
//! lives in `HKCU\Control Panel\NotifyIconSettings\<id>\IsPromoted`; we set it
//! once for our own icon and never override a value the user already chose.

use windows::core::{GUID, PCWSTR};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Registry::*;
use windows::Win32::UI::Shell::{SHGetKnownFolderPath, KNOWN_FOLDER_FLAG};

const ROOT: &str = "Control Panel\\NotifyIconSettings";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Explorer stores paths under known folders as `{FOLDERID}\rest`.
fn resolve_known_folder(path: &str) -> String {
    if let Some(rest) = path.strip_prefix('{') {
        if let Some((guid, tail)) = rest.split_once("}\\") {
            if let Ok(id) = GUID::try_from(guid) {
                unsafe {
                    if let Ok(p) = SHGetKnownFolderPath(&id, KNOWN_FOLDER_FLAG(0), None) {
                        let base = p.to_string().unwrap_or_default();
                        CoTaskMemFree(Some(p.0 as *const _));
                        return format!("{base}\\{tail}");
                    }
                }
            }
        }
    }
    path.to_string()
}

fn read_string(key: HKEY, sub: &[u16], value: &str) -> Option<String> {
    let name = wide(value);
    let mut buf = [0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(
            key,
            PCWSTR(sub.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut size),
        )
    };
    if r != ERROR_SUCCESS {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

fn has_dword(key: HKEY, sub: &[u16], value: &str) -> bool {
    let name = wide(value);
    let mut v = 0u32;
    let mut size = 4u32;
    let r = unsafe {
        RegGetValueW(
            key,
            PCWSTR(sub.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut v as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    r == ERROR_SUCCESS
}

pub enum PinResult {
    /// IsPromoted was missing and has been set: re-add the icon to apply it.
    Pinned,
    /// The user already decided (pinned or hidden it): nothing to do.
    AlreadyDecided,
    /// Explorer has not registered our icon yet (or not Windows 11).
    NotFound,
}

pub fn pin_own_icon() -> PinResult {
    let Ok(exe) = std::env::current_exe() else { return PinResult::NotFound };
    let exe = exe.to_string_lossy().to_lowercase();
    unsafe {
        let mut root = HKEY::default();
        let root_name = wide(ROOT);
        if RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(root_name.as_ptr()), None, KEY_READ | KEY_SET_VALUE, &mut root)
            != ERROR_SUCCESS
        {
            return PinResult::NotFound;
        }
        let mut result = PinResult::NotFound;
        let mut i = 0u32;
        loop {
            let mut name = [0u16; 256];
            let mut len = name.len() as u32;
            let r =
                RegEnumKeyExW(root, i, Some(windows::core::PWSTR(name.as_mut_ptr())), &mut len, None, None, None, None);
            if r != ERROR_SUCCESS {
                break;
            }
            i += 1;
            let sub: Vec<u16> = name[..len as usize].iter().copied().chain(Some(0)).collect();
            let Some(path) = read_string(root, &sub, "ExecutablePath") else { continue };
            if resolve_known_folder(&path).to_lowercase() != exe {
                continue;
            }
            if has_dword(root, &sub, "IsPromoted") {
                result = PinResult::AlreadyDecided;
                continue;
            }
            let one = 1u32;
            let value = wide("IsPromoted");
            let w = RegSetKeyValueW(
                root,
                PCWSTR(sub.as_ptr()),
                PCWSTR(value.as_ptr()),
                REG_DWORD.0,
                Some(&one as *const u32 as *const _),
                4,
            );
            if w == ERROR_SUCCESS {
                crate::log!("tray: pinned icon to the taskbar");
                result = PinResult::Pinned;
            }
        }
        let _ = RegCloseKey(root);
        result
    }
}
