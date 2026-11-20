//! Window capture through Windows.Graphics.Capture.
//!
//! WGC reads the game's composed window surface from DWM. Nothing is injected
//! into the game process, so anti-cheat systems have nothing to object to.

use crate::gpu::Gpu;
use windows::core::{Interface, Result};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::Threading::{CreateEventW, SetEvent};
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

const BUFFERS: i32 = 3;

pub struct Capture {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    device: IDirect3DDevice,
    token: i64,
    size: SizeInt32,
    /// Auto-reset event signalled by WGC whenever a new frame is ready.
    pub event: HANDLE,
}

pub struct CapturedFrame {
    frame: Direct3D11CaptureFrame,
    pub texture: ID3D11Texture2D,
    /// QPC time of the frame in 100 ns units.
    pub time: i64,
    pub width: u32,
    pub height: u32,
}

impl Drop for CapturedFrame {
    fn drop(&mut self) {
        let _ = self.frame.Close();
    }
}

impl Capture {
    pub fn new(gpu: &Gpu, hwnd: HWND) -> Result<Capture> {
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(hwnd)? };
        let dxgi: IDXGIDevice = gpu.device.cast()?;
        let device: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;
        let size = item.Size()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            BUFFERS,
            size,
        )?;
        let session = pool.CreateCaptureSession(&item)?;
        // The real mouse cursor is drawn by Windows on top of our overlay anyway.
        let _ = session.SetIsCursorCaptureEnabled(false);
        // No yellow "being captured" border (Windows 11).
        let _ = session.SetIsBorderRequired(false);
        // WGC throttles delivery by default (~60 fps). Ask for every frame (Windows 11 24H2+).
        let default_interval = session.MinUpdateInterval().map(|t| t.Duration).unwrap_or(-1);
        let set = session.SetMinUpdateInterval(windows::Foundation::TimeSpan { Duration: 10_000 });
        crate::log!(
            "capture: default MinUpdateInterval {:.2} ms, set to 1 ms: {}",
            default_interval as f64 / 1e4,
            if set.is_ok() { "ok" } else { "not supported" }
        );

        let event = unsafe { CreateEventW(None, false, false, None)? };
        let raw = event.0 as usize;
        let token = pool.FrameArrived(&TypedEventHandler::new(move |_, _| {
            unsafe {
                let _ = SetEvent(HANDLE(raw as *mut _));
            }
            Ok(())
        }))?;
        session.StartCapture()?;
        Ok(Capture { pool, session, device, token, size, event })
    }

    /// Returns the next pending frame, if any.
    pub fn try_next(&mut self) -> Result<Option<CapturedFrame>> {
        let frame = match self.pool.TryGetNextFrame() {
            Ok(f) => f,
            Err(_) => return Ok(None),
        };
        let content = frame.ContentSize()?;
        if content.Width != self.size.Width || content.Height != self.size.Height {
            self.size = content;
            self.pool.Recreate(&self.device, DirectXPixelFormat::B8G8R8A8UIntNormalized, BUFFERS, content)?;
        }
        let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
        let texture: ID3D11Texture2D = unsafe { access.GetInterface()? };
        let time = frame.SystemRelativeTime()?.Duration;
        Ok(Some(CapturedFrame {
            frame,
            texture,
            time,
            width: content.Width.max(0) as u32,
            height: content.Height.max(0) as u32,
        }))
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.pool.RemoveFrameArrived(self.token);
        let _ = self.session.Close();
        let _ = self.pool.Close();
        unsafe {
            let _ = CloseHandle(self.event);
        }
    }
}
