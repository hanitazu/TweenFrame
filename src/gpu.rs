//! Direct3D 11 device creation and small resource helpers.

use windows::core::{Interface, Result, PCSTR};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::{
    ID3DBlob, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::HMONITOR;

pub use windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext as Context;

pub struct Gpu {
    pub device: ID3D11Device,
    pub ctx: ID3D11DeviceContext,
    pub factory: IDXGIFactory2,
    pub adapter_name: String,
}

impl Gpu {
    /// Creates a device on the adapter that drives `monitor` (falls back to the
    /// high-performance GPU). Using the display adapter avoids cross-GPU copies.
    pub fn new(monitor: Option<HMONITOR>) -> Result<Gpu> {
        unsafe {
            let factory: IDXGIFactory6 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
            let mut chosen: Option<IDXGIAdapter1> = None;

            if let Some(mon) = monitor {
                let mut i = 0;
                'outer: while let Ok(adapter) = factory.EnumAdapters1(i) {
                    let mut j = 0;
                    while let Ok(output) = adapter.EnumOutputs(j) {
                        if let Ok(desc) = output.GetDesc() {
                            if desc.Monitor == mon {
                                chosen = Some(adapter.clone());
                                break 'outer;
                            }
                        }
                        j += 1;
                    }
                    i += 1;
                }
            }
            if chosen.is_none() {
                chosen = factory.EnumAdapterByGpuPreference(0, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE).ok();
            }
            let adapter = match chosen {
                Some(a) => a,
                None => factory.EnumAdapters1(0)?,
            };
            let desc = adapter.GetDesc1()?;
            let len = desc.Description.iter().position(|&c| c == 0).unwrap_or(desc.Description.len());
            let adapter_name = String::from_utf16_lossy(&desc.Description[..len]);

            let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
            let mut device = None;
            let mut ctx = None;
            let mut got = D3D_FEATURE_LEVEL::default();
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut got),
                Some(&mut ctx),
            )?;
            let device: ID3D11Device = device.unwrap();
            let ctx: ID3D11DeviceContext = ctx.unwrap();

            // Windows.Graphics.Capture touches the device from its own threads.
            if let Ok(mt) = device.cast::<ID3D11Multithread>() {
                let _ = mt.SetMultithreadProtected(true);
            }
            // Ask the scheduler to prioritise our small GPU jobs over the game's
            // big ones (silently ignored where not permitted).
            if let Ok(dxgi) = device.cast::<IDXGIDevice>() {
                let _ = dxgi.SetGPUThreadPriority(7);
            }
            if let Ok(dxgi1) = device.cast::<IDXGIDevice1>() {
                let _ = dxgi1.SetMaximumFrameLatency(1);
            }

            Ok(Gpu { device, ctx, factory: factory.cast()?, adapter_name })
        }
    }

    pub fn compute_shader(&self, bytecode: &[u8]) -> Result<ID3D11ComputeShader> {
        let mut cs = None;
        unsafe { self.device.CreateComputeShader(bytecode, None, Some(&mut cs))? };
        Ok(cs.unwrap())
    }

    pub fn linear_clamp_sampler(&self) -> Result<ID3D11SamplerState> {
        let desc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            MipLODBias: 0.0,
            MaxAnisotropy: 1,
            ComparisonFunc: D3D11_COMPARISON_NEVER,
            BorderColor: [0.0; 4],
            MinLOD: 0.0,
            MaxLOD: f32::MAX,
        };
        let mut s = None;
        unsafe { self.device.CreateSamplerState(&desc, Some(&mut s))? };
        Ok(s.unwrap())
    }
}

/// A 2D texture with the views we need.
pub struct Tex {
    pub tex: ID3D11Texture2D,
    pub srv: ID3D11ShaderResourceView,
    pub uav: Option<ID3D11UnorderedAccessView>,
    pub w: u32,
    pub h: u32,
}

impl Tex {
    pub fn new(gpu: &Gpu, w: u32, h: u32, format: DXGI_FORMAT, uav: bool) -> Result<Tex> {
        let mut bind = D3D11_BIND_SHADER_RESOURCE.0 as u32;
        if uav {
            bind |= D3D11_BIND_UNORDERED_ACCESS.0 as u32;
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w.max(1),
            Height: h.max(1),
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: bind,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        unsafe {
            let mut tex = None;
            gpu.device.CreateTexture2D(&desc, None, Some(&mut tex))?;
            let tex: ID3D11Texture2D = tex.unwrap();
            let mut srv = None;
            gpu.device.CreateShaderResourceView(&tex, None, Some(&mut srv))?;
            let uav = if uav {
                let mut u = None;
                gpu.device.CreateUnorderedAccessView(&tex, None, Some(&mut u))?;
                u
            } else {
                None
            };
            Ok(Tex { tex, srv: srv.unwrap(), uav, w: w.max(1), h: h.max(1) })
        }
    }
}

/// A 64-byte dynamic constant buffer, rewritten before every dispatch.
pub struct ConstBuf {
    pub buf: ID3D11Buffer,
}

impl ConstBuf {
    pub fn new(gpu: &Gpu) -> Result<ConstBuf> {
        let desc = D3D11_BUFFER_DESC {
            ByteWidth: 64,
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            MiscFlags: 0,
            StructureByteStride: 0,
        };
        let mut buf = None;
        unsafe { gpu.device.CreateBuffer(&desc, None, Some(&mut buf))? };
        Ok(ConstBuf { buf: buf.unwrap() })
    }

    /// Writes up to 16 32-bit values.
    pub fn write(&self, ctx: &ID3D11DeviceContext, data: &[u32]) -> Result<()> {
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&self.buf, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))?;
            let n = data.len().min(16);
            std::ptr::copy_nonoverlapping(data.as_ptr(), mapped.pData as *mut u32, n);
            ctx.Unmap(&self.buf, 0);
        }
        Ok(())
    }
}

/// Helper to pack mixed int/float constants.
#[derive(Default)]
pub struct Consts(pub Vec<u32>);

impl Consts {
    pub fn i(mut self, v: i32) -> Self {
        self.0.push(v as u32);
        self
    }
    pub fn f(mut self, v: f32) -> Self {
        self.0.push(v.to_bits());
        self
    }
}

/// Optional GPU timestamp profiler (used by the selftest for tuning).
pub struct Profiler {
    disjoint: ID3D11Query,
    pool: Vec<ID3D11Query>,
    marks: Vec<(&'static str, usize)>,
}

impl Profiler {
    pub fn new(gpu: &Gpu, capacity: usize) -> Result<Profiler> {
        let mk = |kind| -> Result<ID3D11Query> {
            let mut q = None;
            unsafe { gpu.device.CreateQuery(&D3D11_QUERY_DESC { Query: kind, MiscFlags: 0 }, Some(&mut q))? };
            Ok(q.unwrap())
        };
        Ok(Profiler {
            disjoint: mk(D3D11_QUERY_TIMESTAMP_DISJOINT)?,
            pool: (0..capacity).map(|_| mk(D3D11_QUERY_TIMESTAMP)).collect::<Result<_>>()?,
            marks: Vec::new(),
        })
    }

    pub fn begin(&mut self, ctx: &ID3D11DeviceContext) {
        self.marks.clear();
        unsafe { ctx.Begin(&self.disjoint) };
        self.mark(ctx, "start");
    }

    pub fn mark(&mut self, ctx: &ID3D11DeviceContext, label: &'static str) {
        let i = self.marks.len();
        if i < self.pool.len() {
            unsafe { ctx.End(&self.pool[i]) };
            self.marks.push((label, i));
        }
    }

    /// Ends the frame and returns (label, milliseconds since previous mark).
    pub fn end(&mut self, ctx: &ID3D11DeviceContext) -> Vec<(&'static str, f64)> {
        unsafe {
            ctx.End(&self.disjoint);
            ctx.Flush();
            let mut d = D3D11_QUERY_DATA_TIMESTAMP_DISJOINT::default();
            while d.Frequency == 0 {
                let _ =
                    ctx.GetData(&self.disjoint, Some(&mut d as *mut _ as *mut _), std::mem::size_of_val(&d) as u32, 0);
            }
            let mut stamps = Vec::new();
            for &(_, i) in &self.marks {
                let mut v = 0u64;
                while v == 0 {
                    let _ = ctx.GetData(&self.pool[i], Some(&mut v as *mut u64 as *mut _), 8, 0);
                }
                stamps.push(v);
            }
            let f = d.Frequency as f64;
            (1..stamps.len())
                .map(|k| (self.marks[k].0, (stamps[k] as f64 - stamps[k - 1] as f64) / f * 1000.0))
                .collect()
        }
    }
}

/// Compiles HLSL source with the system `d3dcompiler_47.dll`.
pub fn compile(src: &str, name: &str, entry: &str, target: &str) -> Result<Vec<u8>> {
    let name_c = std::ffi::CString::new(name).unwrap();
    let entry_c = std::ffi::CString::new(entry).unwrap();
    let target_c = std::ffi::CString::new(target).unwrap();
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    let res = unsafe {
        D3DCompile(
            src.as_ptr() as *const _,
            src.len(),
            PCSTR(name_c.as_ptr() as *const u8),
            None,
            None,
            PCSTR(entry_c.as_ptr() as *const u8),
            PCSTR(target_c.as_ptr() as *const u8),
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = res {
        if let Some(errs) = errors {
            let msg = unsafe { std::slice::from_raw_parts(errs.GetBufferPointer() as *const u8, errs.GetBufferSize()) };
            crate::log!("shader {name}:{entry} failed: {}", String::from_utf8_lossy(msg));
        }
        return Err(e);
    }
    let code = code.unwrap();
    let bytes = unsafe { std::slice::from_raw_parts(code.GetBufferPointer() as *const u8, code.GetBufferSize()) };
    Ok(bytes.to_vec())
}

/// Performance-counter time in 100 ns units (same clock domain as
/// `Direct3D11CaptureFrame::SystemRelativeTime`).
pub fn now_100ns() -> i64 {
    use std::sync::OnceLock;
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    static FREQ: OnceLock<i64> = OnceLock::new();
    let freq = *FREQ.get_or_init(|| {
        let mut f = 0i64;
        unsafe {
            let _ = QueryPerformanceFrequency(&mut f);
        }
        f.max(1)
    });
    let mut c = 0i64;
    unsafe {
        let _ = QueryPerformanceCounter(&mut c);
    }
    ((c as i128 * 10_000_000) / freq as i128) as i64
}
