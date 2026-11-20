//! Click-through, always-on-top window with a flip-model swap chain that sits
//! exactly over the game's client area and shows the generated frames.

use crate::gpu::Gpu;
use std::sync::Once;
use windows::core::{w, Interface, Result};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D11::{ID3D11RenderTargetView, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

const CLASS: windows::core::PCWSTR = w!("TweenFrameOverlay");

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn register_class() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: CLASS,
            hCursor: HCURSOR::default(),
            ..Default::default()
        };
        RegisterClassExW(&wc);
    });
}

pub struct Overlay {
    pub hwnd: HWND,
    swapchain: IDXGISwapChain2,
    pub waitable: HANDLE,
    pub backbuffer: ID3D11Texture2D,
    pub rtv: ID3D11RenderTargetView,
    pub visible: bool,
    rect: RECT,
}

impl Overlay {
    pub fn new(gpu: &Gpu, rect: RECT) -> Result<Overlay> {
        register_class();
        let (w, h) = ((rect.right - rect.left) as u32, (rect.bottom - rect.top) as u32);
        unsafe {
            let hinst = GetModuleHandleW(None)?;
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE
                    | WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_NOREDIRECTIONBITMAP,
                CLASS,
                w!("TweenFrame Overlay"),
                WS_POPUP,
                rect.left,
                rect.top,
                w as i32,
                h as i32,
                None,
                None,
                Some(hinst.into()),
                None,
            )?;
            // Fully opaque; layered + transparent makes every click fall through to the game.
            SetLayeredWindowAttributes(hwnd, windows::Win32::Foundation::COLORREF(0), 255, LWA_ALPHA)?;

            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: w,
                Height: h,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                Stereo: false.into(),
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_NONE,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                Flags: DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32,
            };
            let sc1 = gpu.factory.CreateSwapChainForHwnd(&gpu.device, hwnd, &desc, None, None)?;
            let _ = gpu.factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER | DXGI_MWA_NO_WINDOW_CHANGES);
            let swapchain: IDXGISwapChain2 = sc1.cast()?;
            swapchain.SetMaximumFrameLatency(1)?;
            let waitable = swapchain.GetFrameLatencyWaitableObject();
            let backbuffer: ID3D11Texture2D = swapchain.GetBuffer(0)?;
            let mut rtv = None;
            gpu.device.CreateRenderTargetView(&backbuffer, None, Some(&mut rtv))?;
            Ok(Overlay { hwnd, swapchain, waitable, backbuffer, rtv: rtv.unwrap(), visible: false, rect })
        }
    }

    pub fn show(&mut self) {
        if !self.visible {
            unsafe {
                let r = self.rect;
                let _ = SetWindowPos(
                    self.hwnd,
                    Some(HWND_TOPMOST),
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
            }
            self.visible = true;
        }
    }

    pub fn hide(&mut self) {
        if self.visible {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
            self.visible = false;
        }
    }

    /// Re-asserts topmost order (other topmost windows may have jumped above us).
    pub fn keep_on_top(&self) {
        if self.visible {
            unsafe {
                let _ = SetWindowPos(
                    self.hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_NOOWNERZORDER,
                );
            }
        }
    }

    /// Presents on the next vertical blank (no tearing).
    pub fn present(&self) -> Result<()> {
        unsafe { self.swapchain.Present(1, DXGI_PRESENT(0)).ok() }
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.waitable);
            let _ = DestroyWindow(self.hwnd);
        }
    }
}
