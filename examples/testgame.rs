//! A stand-in "game" for end-to-end tests: a borderless window with the RAGE
//! window class (`grcWindow`) that renders a moving scene at a fixed frame rate.
//!
//!   cargo run --release --example testgame -- [fps] [seconds] [x y w h]

use std::time::{Duration, Instant};
use tweenframe::gpu::{compile, Gpu};
use windows::core::{w, Interface, Result};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::WindowsAndMessaging::*;

const SHADER: &str = r#"
cbuffer CB : register(b0) { float gTime; float2 gSize; float pad; };
void VSMain(uint vid : SV_VertexID, out float4 pos : SV_Position) {
    float2 uv = float2((vid << 1) & 2, vid & 2);
    pos = float4(uv * float2(2, -2) + float2(-1, 1), 0, 1);
}
float hash(float2 p) { return frac(sin(dot(p, float2(127.1, 311.7))) * 43758.5453); }
float noise(float2 p) {
    float2 i = floor(p), f = frac(p); f = f * f * (3 - 2 * f);
    return lerp(lerp(hash(i), hash(i + float2(1, 0)), f.x), lerp(hash(i + float2(0, 1)), hash(i + 1), f.x), f.y);
}
float4 PSMain(float4 sv : SV_Position) : SV_Target {
    float2 p = sv.xy;
    float2 cam = float2(gTime * 900.0, gTime * 120.0);          // fast camera pan (px/s)
    float2 q = (p + cam) / 40.0;
    float n = noise(q) * 0.5 + noise(q * 2.1) * 0.3 + noise(q * 4.3) * 0.2;
    float3 col = lerp(float3(0.10, 0.18, 0.30), float3(0.85, 0.75, 0.55), n);
    float grid = step(0.94, frac(q.x * 0.5)) + step(0.94, frac(q.y * 0.5));
    col = lerp(col, float3(0.95, 0.95, 0.95), saturate(grid) * 0.6);
    // car moving the other way
    float2 c = float2(gSize.x * 0.5 + sin(gTime * 0.9) * gSize.x * 0.35, gSize.y * 0.62);
    float2 d = p - c;
    if (abs(d.x) < 110 && abs(d.y) < 45) {
        col = float3(0.8, 0.1, 0.1) * (0.7 + 0.3 * step(0.5, frac((d.x + d.y) / 24.0)));
    }
    // static HUD: minimap + text
    if (p.x > 20 && p.x < 260 && p.y > gSize.y - 180 && p.y < gSize.y - 20) {
        bool border = p.x < 24 || p.x > 256 || p.y < gSize.y - 176 || p.y > gSize.y - 24;
        col = border ? float3(1, 1, 1) : col * 0.3 + float3(0.05, 0.2, 0.05);
    }
    if (p.y > 30 && p.y < 44 && p.x > gSize.x - 300 && p.x < gSize.x - 30 && fmod(floor(p.x / 9.0), 3.0) != 0)
        col = float3(1, 1, 1);
    return float4(col, 1);
}
"#;

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_DESTROY {
        PostQuitMessage(0);
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let num = |i: usize, d: f64| args.get(i).and_then(|s| s.parse::<f64>().ok()).unwrap_or(d);
    let fps = num(1, 100.0);
    let seconds = num(2, 15.0);
    let (x, y, w, h) = (num(3, 200.0) as i32, num(4, 150.0) as i32, num(5, 1280.0) as u32, num(6, 720.0) as u32);

    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let hinst = GetModuleHandleW(None)?;
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: w!("grcWindow"),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("grcWindow"),
            w!("TweenFrame Test Game"),
            WS_POPUP | WS_VISIBLE,
            x,
            y,
            w as i32,
            h as i32,
            None,
            None,
            Some(hinst.into()),
            None,
        )?;

        let gpu = Gpu::new(None)?;
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
            Flags: 0,
        };
        let sc = gpu.factory.CreateSwapChainForHwnd(&gpu.device, hwnd, &desc, None, None)?;
        let bb: ID3D11Texture2D = sc.GetBuffer(0)?;
        let mut rtv = None;
        gpu.device.CreateRenderTargetView(&bb, None, Some(&mut rtv))?;
        let rtv = rtv.unwrap();
        let vsb = compile(SHADER, "testgame", "VSMain", "vs_5_0")?;
        let psb = compile(SHADER, "testgame", "PSMain", "ps_5_0")?;
        let (mut vs, mut ps) = (None, None);
        gpu.device.CreateVertexShader(&vsb, None, Some(&mut vs))?;
        gpu.device.CreatePixelShader(&psb, None, Some(&mut ps))?;
        let cb = tweenframe::gpu::ConstBuf::new(&gpu)?;

        let start = Instant::now();
        let frame_time = Duration::from_secs_f64(1.0 / fps);
        let mut next = Instant::now();
        let mut frames = 0u64;
        let mut occluded = 0u64;
        let mut last_report = Instant::now();
        let mut report_frames = 0u64;
        loop {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    return Ok(());
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            let t = start.elapsed().as_secs_f32();
            if t as f64 > seconds {
                break;
            }
            cb.write(&gpu.ctx, &tweenframe::gpu::Consts::default().f(t).f(w as f32).f(h as f32).f(0.0).0)?;
            let ctx = &gpu.ctx;
            ctx.RSSetViewports(Some(&[D3D11_VIEWPORT {
                Width: w as f32,
                Height: h as f32,
                MaxDepth: 1.0,
                ..Default::default()
            }]));
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.VSSetShader(vs.as_ref(), None);
            ctx.PSSetShader(ps.as_ref(), None);
            ctx.PSSetConstantBuffers(0, Some(&[Some(cb.buf.clone())]));
            ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            ctx.Draw(3, 0);
            let hr = sc.Present(0, DXGI_PRESENT(0));
            if hr == DXGI_STATUS_OCCLUDED {
                occluded += 1;
            }
            frames += 1;
            report_frames += 1;
            if last_report.elapsed() >= Duration::from_secs(2) {
                println!(
                    "testgame: {:.1} fps, occluded presents so far: {occluded}",
                    report_frames as f64 / last_report.elapsed().as_secs_f64()
                );
                last_report = Instant::now();
                report_frames = 0;
            }
            next += frame_time;
            let now = Instant::now();
            if next > now {
                // coarse sleep then spin for accuracy
                let left = next - now;
                if left > Duration::from_millis(2) {
                    std::thread::sleep(left - Duration::from_millis(1));
                }
                while Instant::now() < next {
                    std::hint::spin_loop();
                }
            } else {
                next = now;
            }
        }
        println!("testgame: done, {frames} frames, {occluded} occluded");
        let _ = gpu.device.cast::<ID3D11Device>();
        let _ = DestroyWindow(hwnd);
    }
    Ok(())
}
