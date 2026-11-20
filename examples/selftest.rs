//! Offline quality + performance test of the frame-generation pipeline.
//!
//! Renders a synthetic "game" scene (detailed photo background panning fast,
//! two textured objects moving against it, a static HUD) at arbitrary times, so
//! generated frames can be compared against the true in-between frames.
//!
//!   cargo run --release --example selftest -- [out_dir] [width height]
//!
//! Tuning via env vars: TF_MAXW, TF_LAMBDA, TF_ITERS, TF_MEDIAN, TF_ZB,
//! TF_SHARP, TF_OCCLO, TF_OCCHI, TF_RADIUS, TF_CHECKER=1 (periodic stress test).

use std::time::Instant;
use tweenframe::framegen::FrameGen;
use tweenframe::gpu::{Gpu, Profiler};
use windows::core::Result;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::*;

struct Image {
    w: usize,
    h: usize,
    px: Vec<[f32; 3]>,
}

impl Image {
    #[allow(clippy::needless_range_loop)]
    fn sample(&self, x: f32, y: f32) -> [f32; 3] {
        let x = x.rem_euclid(self.w as f32);
        let y = y.rem_euclid(self.h as f32);
        let x0 = x.floor() as usize % self.w;
        let y0 = y.floor() as usize % self.h;
        let x1 = (x0 + 1) % self.w;
        let y1 = (y0 + 1) % self.h;
        let fx = x - x.floor();
        let fy = y - y.floor();
        let p = |xx: usize, yy: usize| self.px[yy * self.w + xx];
        let mut out = [0.0; 3];
        for c in 0..3 {
            let a = p(x0, y0)[c] * (1.0 - fx) + p(x1, y0)[c] * fx;
            let b = p(x0, y1)[c] * (1.0 - fx) + p(x1, y1)[c] * fx;
            out[c] = a * (1.0 - fy) + b * fy;
        }
        out
    }
}

fn load_first(paths: &[&str]) -> Image {
    for path in paths {
        if let Ok(img) = load_wic(path) {
            println!("texture: {path} ({}x{})", img.w, img.h);
            return img;
        }
    }
    println!("texture: procedural");
    let (w, h) = (1024, 1024);
    let mut px = vec![[0.0; 3]; w * h];
    for y in 0..h {
        for x in 0..w {
            let fx = x as f32 / 37.0;
            let fy = y as f32 / 23.0;
            let v = ((fx.sin() * fy.cos() + (fx * 0.31 + fy * 0.7).sin()) * 0.25 + 0.5).clamp(0.0, 1.0);
            px[y * w + x] = [v, (v * 0.7 + 0.2 * ((x ^ y) & 16) as f32 / 16.0).min(1.0), 1.0 - v];
        }
    }
    Image { w, h, px }
}

fn load_wic(path: &str) -> Result<Image> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let factory: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let wpath: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        let decoder = factory.CreateDecoderFromFilename(
            windows::core::PCWSTR(wpath.as_ptr()),
            None,
            windows::Win32::Foundation::GENERIC_READ,
            WICDecodeMetadataCacheOnDemand,
        )?;
        let frame = decoder.GetFrame(0)?;
        let conv = factory.CreateFormatConverter()?;
        conv.Initialize(
            &frame,
            &GUID_WICPixelFormat24bppRGB,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )?;
        let (mut w, mut h) = (0u32, 0u32);
        conv.GetSize(&mut w, &mut h)?;
        let mut buf = vec![0u8; (w * h * 3) as usize];
        conv.CopyPixels(std::ptr::null(), w * 3, &mut buf)?;
        let px = buf.chunks(3).map(|c| [c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0]).collect();
        Ok(Image { w: w as usize, h: h as usize, px })
    }
}

/// Offset of the most detailed `win`-sized region (highest mean gradient).
fn busiest_region(img: &Image, win: usize) -> [f32; 2] {
    let mut best = (0.0f32, [0.0f32; 2]);
    let steps = 6;
    for gy in 0..steps {
        for gx in 0..steps {
            let ox = gx * img.w.saturating_sub(win) / (steps - 1);
            let oy = gy * img.h.saturating_sub(win) / (steps - 1);
            let mut e = 0.0;
            for y in (oy..(oy + win).min(img.h - 1)).step_by(4) {
                for x in (ox..(ox + win).min(img.w - 1)).step_by(4) {
                    let a = img.px[y * img.w + x];
                    let b = img.px[y * img.w + x + 1];
                    let c = img.px[(y + 1) * img.w + x];
                    e += (a[1] - b[1]).abs() + (a[1] - c[1]).abs();
                }
            }
            if e > best.0 {
                best = (e, [ox as f32, oy as f32]);
            }
        }
    }
    best.1
}

struct Scene {
    bg: Image,
    obj: Image,
    checker: bool,
    bg_off: [f32; 2],
    obj_off: [f32; 2],
}

impl Scene {
    /// Object centres/radii at time t (frames).
    fn objects(&self, w: usize, h: usize, t: f32) -> [([f32; 2], f32); 2] {
        let s = w as f32 / 2560.0;
        [
            ([w as f32 * 0.55 - t * 27.0 * s, h as f32 * 0.6 + t * 4.0 * s], 150.0 * s),
            ([w as f32 * 0.25 + t * 6.0 * s, h as f32 * 0.35 - t * 19.0 * s], 90.0 * s),
        ]
    }

    /// The scene at continuous time `t` (in frames). Returns BGRA8.
    fn render(&self, w: usize, h: usize, t: f32) -> Vec<u8> {
        let s = w as f32 / 2560.0;
        let cam = [t * 41.0 * s, t * 9.0 * s];
        let objs = self.objects(w, h, t);
        let mut out = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let (xf, yf) = (x as f32 + 0.5, y as f32 + 0.5);
                let mut c =
                    self.bg.sample((xf + cam[0]) / s * 1.2 + self.bg_off[0], (yf + cam[1]) / s * 1.2 + self.bg_off[1]);
                for (k, (oc, r)) in objs.iter().enumerate() {
                    let dx = xf - oc[0];
                    let dy = yf - oc[1];
                    if dx * dx + dy * dy < r * r {
                        c = if self.checker && k == 0 {
                            let u = dx / r;
                            let v = dy / r;
                            if ((u * 9.0).floor() as i32 + (v * 9.0).floor() as i32) & 1 == 0 {
                                [0.9, 0.15, 0.1]
                            } else {
                                [0.95, 0.85, 0.2]
                            }
                        } else {
                            let tint = if k == 0 { [1.1, 0.8, 0.7] } else { [0.7, 0.9, 1.2] };
                            let p = self.obj.sample(
                                dx / s + 300.0 * k as f32 + self.obj_off[0] + 200.0,
                                dy / s + self.obj_off[1] + 200.0,
                            );
                            [(p[0] * tint[0]).min(1.0), (p[1] * tint[1]).min(1.0), (p[2] * tint[2]).min(1.0)]
                        };
                    }
                }
                // static HUD: minimap box bottom-left + text bars top-right
                let mm = (40.0 * s, h as f32 - 300.0 * s, 360.0 * s, h as f32 - 40.0 * s);
                if xf > mm.0 && xf < mm.2 && yf > mm.1 && yf < mm.3 {
                    let border =
                        xf < mm.0 + 6.0 * s || xf > mm.2 - 6.0 * s || yf < mm.1 + 6.0 * s || yf > mm.3 - 6.0 * s;
                    c = if border { [1.0, 1.0, 1.0] } else { [c[0] * 0.3 + 0.1, c[1] * 0.3 + 0.25, c[2] * 0.3 + 0.1] };
                }
                for row in 0..4 {
                    let ty = 50.0 * s + row as f32 * 34.0 * s;
                    if yf > ty && yf < ty + 18.0 * s && xf > w as f32 - 520.0 * s && xf < w as f32 - 60.0 * s {
                        let glyph = (((xf - (w as f32 - 520.0 * s)) / (11.0 * s)) as i32 + row) % 3 != 0;
                        if glyph {
                            c = [1.0, 1.0, 1.0];
                        }
                    }
                }
                let o = (y * w + x) * 4;
                out[o] = (c[2].clamp(0.0, 1.0) * 255.0) as u8;
                out[o + 1] = (c[1].clamp(0.0, 1.0) * 255.0) as u8;
                out[o + 2] = (c[0].clamp(0.0, 1.0) * 255.0) as u8;
                out[o + 3] = 255;
            }
        }
        out
    }
}

fn tex_desc(w: u32, h: u32, usage: D3D11_USAGE, bind: u32, cpu: u32) -> D3D11_TEXTURE2D_DESC {
    D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: usage,
        BindFlags: bind,
        CPUAccessFlags: cpu,
        MiscFlags: 0,
    }
}

fn upload(gpu: &Gpu, w: u32, h: u32, data: &[u8]) -> Result<ID3D11Texture2D> {
    let desc = tex_desc(w, h, D3D11_USAGE_DEFAULT, D3D11_BIND_SHADER_RESOURCE.0 as u32, 0);
    let init = D3D11_SUBRESOURCE_DATA { pSysMem: data.as_ptr() as *const _, SysMemPitch: w * 4, SysMemSlicePitch: 0 };
    let mut tex = None;
    unsafe { gpu.device.CreateTexture2D(&desc, Some(&init), Some(&mut tex))? };
    Ok(tex.unwrap())
}

fn readback(gpu: &Gpu, tex: &ID3D11Texture2D, w: u32, h: u32) -> Result<Vec<u8>> {
    let desc = tex_desc(w, h, D3D11_USAGE_STAGING, 0, D3D11_CPU_ACCESS_READ.0 as u32);
    unsafe {
        let mut st = None;
        gpu.device.CreateTexture2D(&desc, None, Some(&mut st))?;
        let st: ID3D11Texture2D = st.unwrap();
        gpu.ctx.CopyResource(&st, tex);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        gpu.ctx.Map(&st, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
        let mut out = vec![0u8; (w * h * 4) as usize];
        for y in 0..h as usize {
            let src = std::slice::from_raw_parts((m.pData as *const u8).add(y * m.RowPitch as usize), w as usize * 4);
            out[y * w as usize * 4..(y + 1) * w as usize * 4].copy_from_slice(src);
        }
        gpu.ctx.Unmap(&st, 0);
        Ok(out)
    }
}

/// PSNR over pixels where `mask` is true (or all if None).
fn psnr(a: &[u8], b: &[u8], w: usize, mask: Option<&dyn Fn(usize, usize) -> bool>) -> f64 {
    let mut se = 0.0f64;
    let mut n = 0usize;
    for i in (0..a.len()).step_by(4) {
        let p = i / 4;
        if let Some(m) = mask {
            if !m(p % w, p / w) {
                continue;
            }
        }
        for c in 0..3 {
            let d = a[i + c] as f64 - b[i + c] as f64;
            se += d * d;
            n += 1;
        }
    }
    let mse = se / n.max(1) as f64;
    10.0 * (255.0 * 255.0 / mse.max(1e-9)).log10()
}

fn save_png(path: &std::path::Path, w: u32, h: u32, bgra: &[u8]) {
    let rgb: Vec<u8> = bgra.chunks(4).flat_map(|p| [p[2], p[1], p[0]]).collect();
    let file = std::fs::File::create(path).unwrap();
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&rgb).unwrap();
}

fn crop(bgra: &[u8], w: u32, x0: u32, y0: u32, cw: u32, ch: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((cw * ch * 4) as usize);
    for y in y0..y0 + ch {
        let s = ((y * w + x0) * 4) as usize;
        out.extend_from_slice(&bgra[s..s + (cw * 4) as usize]);
    }
    out
}

fn env_f(name: &str) -> Option<f32> {
    std::env::var(name).ok().and_then(|v| v.parse().ok())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let out_dir = std::path::PathBuf::from(args.get(1).cloned().unwrap_or_else(|| "selftest_out".into()));
    let w: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(2560);
    let h: u32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(1440);
    std::fs::create_dir_all(&out_dir).unwrap();

    let gpu = Gpu::new(None)?;
    println!("adapter: {}", gpu.adapter_name);
    let bg = load_first(&[r"C:\Windows\Web\Screen\img102.jpg", r"C:\Windows\Web\Wallpaper\Windows\img0.jpg"]);
    let obj = load_first(&[r"C:\Windows\Web\Wallpaper\ThemeB\img24.jpg", r"C:\Windows\Web\Screen\img100.jpg"]);
    let bg_off = busiest_region(&bg, 3200);
    let obj_off = busiest_region(&obj, 800);
    println!("busiest regions: bg {bg_off:?}, obj {obj_off:?}");
    let scene = Scene { bg, obj, checker: std::env::var("TF_CHECKER").is_ok(), bg_off, obj_off };

    let maxw = env_f("TF_MAXW").map(|v| v as u32).unwrap_or(720);
    let t0 = Instant::now();
    let mut fg = FrameGen::new(&gpu, w, h, maxw)?;
    println!("pipeline init: {:?}", t0.elapsed());
    if let Some(v) = env_f("TF_LAMBDA") {
        fg.params.lambda = v;
    }
    if let Some(v) = env_f("TF_ITERS") {
        fg.params.iters = v as i32;
    }
    if let Some(v) = env_f("TF_ZB") {
        fg.params.zero_bias = v;
    }
    if let Some(v) = env_f("TF_SHARP") {
        fg.params.sharp = v;
    }
    if let Some(v) = env_f("TF_OCCLO") {
        fg.params.occ_lo = v;
    }
    if let Some(v) = env_f("TF_OCCHI") {
        fg.params.occ_hi = v;
    }
    if let Some(v) = env_f("TF_RADIUS") {
        fg.params.search_radius = v;
    }
    if std::env::var("TF_MEDIAN").is_ok() {
        fg.params.median_all_levels = true;
    }
    println!("params: {:?} maxw={maxw}", fg.params);

    let desc =
        tex_desc(w, h, D3D11_USAGE_DEFAULT, (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32, 0);
    let mut rt = None;
    unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut rt))? };
    let rt: ID3D11Texture2D = rt.unwrap();
    let mut rtv = None;
    unsafe { gpu.device.CreateRenderTargetView(&rt, None, Some(&mut rtv))? };
    let rtv = rtv.unwrap();

    // Three real frames 0,1,2 so the second pair also has a temporal prior.
    let frames: Vec<Vec<u8>> = (0..3).map(|i| scene.render(w as usize, h as usize, i as f32)).collect();
    let texs: Vec<ID3D11Texture2D> = frames.iter().map(|f| upload(&gpu, w, h, f)).collect::<Result<_>>()?;
    for (i, t) in texs.iter().enumerate() {
        fg.push_frame(t, None, i as i64 * 100_000)?;
    }

    let (mut sg, mut sb, mut so, mut n) = (0.0, 0.0, 0.0, 0.0);
    for &phase in &[1.0f32 / 3.0, 0.5, 2.0 / 3.0] {
        fg.render_latest_pair(phase, &rtv, &rt)?;
        let gen = readback(&gpu, &rt, w, h)?;
        let tt = 1.0 + phase;
        let truth = scene.render(w as usize, h as usize, tt);
        let blend: Vec<u8> = frames[1]
            .iter()
            .zip(&frames[2])
            .map(|(&a, &b)| (a as f32 * (1.0 - phase) + b as f32 * phase).round() as u8)
            .collect();
        // object neighbourhood mask (objects at t, enlarged)
        let objs = scene.objects(w as usize, h as usize, tt);
        let mask = |x: usize, y: usize| {
            objs.iter().any(|(c, r)| {
                let dx = x as f32 - c[0];
                let dy = y as f32 - c[1];
                dx * dx + dy * dy < (r * 1.3) * (r * 1.3)
            })
        };
        let pg = psnr(&gen, &truth, w as usize, None);
        let pb = psnr(&blend, &truth, w as usize, None);
        let po = psnr(&gen, &truth, w as usize, Some(&mask));
        let pbo = psnr(&blend, &truth, w as usize, Some(&mask));
        println!("phase {phase:.3}: generated {pg:.2} dB (objects {po:.2}) | cross-fade {pb:.2} dB (objects {pbo:.2})");
        sg += pg;
        sb += pb;
        so += po;
        n += 1.0;
        let tag = format!("{:03}", (phase * 1000.0) as u32);
        let (cw, ch) = (w / 4, h / 4);
        let ox = (objs[0].0[0] as u32).saturating_sub(cw / 2).min(w - cw);
        let oy = (objs[0].0[1] as u32).saturating_sub(ch / 2).min(h - ch);
        let mut side = crop(&gen, w, ox, oy, cw, ch);
        side.extend(crop(&truth, w, ox, oy, cw, ch));
        save_png(&out_dir.join(format!("zoom_obj_{tag}.png")), cw, ch * 2, &side);
        let mut side = crop(&gen, w, 0, h - ch, cw, ch);
        side.extend(crop(&truth, w, 0, h - ch, cw, ch));
        save_png(&out_dir.join(format!("zoom_hud_{tag}.png")), cw, ch * 2, &side);
        if phase == 0.5 {
            save_png(&out_dir.join("gen_500.png"), w, h, &gen);
        }
    }
    println!("MEAN: generated {:.2} dB (objects {:.2}) vs cross-fade {:.2} dB", sg / n, so / n, sb / n);

    // ---- GPU timing ----
    let iters = 40;
    let mut prof = Profiler::new(&gpu, 32)?;
    let mut acc: Vec<(&'static str, f64)> = Vec::new();
    fg.profiler = Some(Profiler::new(&gpu, 32)?);
    for i in 0..iters {
        let mut p = fg.profiler.take().unwrap();
        p.begin(&gpu.ctx);
        fg.profiler = Some(p);
        fg.push_frame(&texs[i % 3], None, (3 + i as i64) * 100_000)?;
        let mut p = fg.profiler.take().unwrap();
        let r = p.end(&gpu.ctx);
        fg.profiler = Some(p);
        if acc.is_empty() {
            acc = r.iter().map(|&(l, _)| (l, 0.0)).collect();
        }
        for (k, &(_, ms)) in r.iter().enumerate() {
            acc[k].1 += ms / iters as f64;
        }
    }
    fg.profiler = None;
    let per_real: f64 = acc.iter().map(|a| a.1).sum();
    for (l, ms) in &acc {
        println!("  {l:<18} {ms:.3} ms");
    }
    prof.begin(&gpu.ctx);
    for i in 0..iters {
        fg.render_latest_pair(0.33 + (i % 2) as f32 * 0.33, &rtv, &rt)?;
    }
    prof.mark(&gpu.ctx, "synth");
    let r = prof.end(&gpu.ctx);
    let per_gen = r[0].1 / iters as f64;
    println!("GPU per real frame:      {per_real:.3} ms");
    println!("GPU per generated frame: {per_gen:.3} ms");
    let load = 100.0 * per_real + 200.0 * per_gen;
    println!("GPU load at 100 fps base, 3x: {load:.1} ms/s ({:.1}%)", load / 10.0);
    Ok(())
}
