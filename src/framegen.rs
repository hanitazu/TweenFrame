//! The frame-generation pipeline: luma pyramid -> bidirectional optical flow ->
//! motion-compensated synthesis. Everything stays on the GPU.

use crate::gpu::{compile, ConstBuf, Consts, Gpu, Profiler, Tex};
use std::sync::OnceLock;
use windows::core::Result;
use windows::Win32::Graphics::Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

const PYRAMID_SRC: &str = include_str!("shaders/pyramid.hlsl");
const FLOW_SRC: &str = include_str!("shaders/flow.hlsl");
const MEDIAN_SRC: &str = include_str!("shaders/median.hlsl");
const COMPOSITE_SRC: &str = include_str!("shaders/composite.hlsl");

struct Bytecode {
    pyr_color: Vec<u8>,
    pyr_luma: Vec<u8>,
    flow: Vec<u8>,
    median: Vec<u8>,
    vs: Vec<u8>,
    ps: Vec<u8>,
}

/// Shaders are compiled once per process; device objects are created per session.
fn bytecode() -> Result<&'static Bytecode> {
    static CACHE: OnceLock<Bytecode> = OnceLock::new();
    if let Some(b) = CACHE.get() {
        return Ok(b);
    }
    let b = Bytecode {
        pyr_color: compile(PYRAMID_SRC, "pyramid.hlsl", "CSFromColor", "cs_5_0")?,
        pyr_luma: compile(PYRAMID_SRC, "pyramid.hlsl", "CSFromLuma", "cs_5_0")?,
        flow: compile(FLOW_SRC, "flow.hlsl", "CSMain", "cs_5_0")?,
        median: compile(MEDIAN_SRC, "median.hlsl", "CSMain", "cs_5_0")?,
        vs: compile(COMPOSITE_SRC, "composite.hlsl", "VSMain", "vs_5_0")?,
        ps: compile(COMPOSITE_SRC, "composite.hlsl", "PSMain", "ps_5_0")?,
    };
    Ok(CACHE.get_or_init(|| b))
}

/// Tunables of the algorithm (defaults chosen with the `selftest` example).
#[derive(Clone, Copy, Debug)]
pub struct Params {
    /// Exhaustive search radius on the coarsest level (in coarse pixels).
    pub search_radius: f32,
    /// Smoothness penalty per level pixel of deviation from the coarser estimate.
    pub lambda: f32,
    /// Cost multiplier for the zero vector (< 1 favours static content).
    pub zero_bias: f32,
    /// Max one-pixel descent steps per level.
    pub iters: i32,
    /// Also median-filter the intermediate levels (slower, slightly more robust).
    pub median_all_levels: bool,
    /// Candidate selectivity in the synthesis pass.
    pub sharp: f32,
    /// Mismatch range in which synthesis switches from cross-fade to nearest frame.
    pub occ_lo: f32,
    pub occ_hi: f32,
    /// Debug: tint generated frames magenta (0 or 1).
    pub debug_tint: f32,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            search_radius: 6.0,
            lambda: 0.0002,
            zero_bias: 0.97,
            iters: 3,
            median_all_levels: false,
            sharp: 150.0,
            occ_lo: 0.03,
            occ_hi: 0.10,
            debug_tint: 0.0,
        }
    }
}

struct Frame {
    color: Tex,
    /// luma[i] is pyramid level i + 1 (half resolution, quarter, ...).
    luma: Vec<Tex>,
    t: i64,
    id: u64,
    /// Luma pyramid is up to date for this frame.
    pyr_ok: bool,
}

struct PairFlow {
    fwd: Tex,
    bwd: Tex,
    /// Flow between this slot's frame and its predecessor is valid.
    ok: bool,
}

struct LevelScratch {
    raw_f: Tex,
    raw_b: Tex,
    filt_f: Tex,
    filt_b: Tex,
}

/// What `render` decided to show.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Shown {
    Real(u64),
    Generated { a: u64, b: u64, phase_q: u32 },
}

pub struct FrameGen {
    ctx: ID3D11DeviceContext,
    cs_pyr_color: ID3D11ComputeShader,
    cs_pyr_luma: ID3D11ComputeShader,
    cs_flow: ID3D11ComputeShader,
    cs_median: ID3D11ComputeShader,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    cb: ConstBuf,
    pub w: u32,
    pub h: u32,
    final_level: u32,
    coarsest: u32,
    frames: Vec<Frame>,
    flows: Vec<PairFlow>,
    scratch: Vec<Option<LevelScratch>>,
    newest: Option<usize>,
    count: u64,
    next_id: u64,
    pub params: Params,
    pub profiler: Option<Profiler>,
}

fn level_size(full: u32, level: u32) -> u32 {
    let mut s = full;
    for _ in 0..level {
        s = s.div_ceil(2);
    }
    s.max(1)
}

impl FrameGen {
    /// `max_flow_width`: the flow field is computed on the first pyramid level
    /// not wider than this (720 -> 1/4 resolution at 1440p, 1/8 at 4K).
    pub fn new(gpu: &Gpu, w: u32, h: u32, max_flow_width: u32) -> Result<FrameGen> {
        let bc = bytecode()?;
        let dev = &gpu.device;

        let mut final_level = 1;
        while level_size(w, final_level) > max_flow_width {
            final_level += 1;
        }
        let mut coarsest = final_level + 1;
        while level_size(w, coarsest) > 100 {
            coarsest += 1;
        }

        let mut frames = Vec::new();
        for _ in 0..3 {
            let color = Tex::new(gpu, w, h, DXGI_FORMAT_B8G8R8A8_UNORM, false)?;
            let mut luma = Vec::new();
            for l in 1..=coarsest {
                luma.push(Tex::new(gpu, level_size(w, l), level_size(h, l), DXGI_FORMAT_R16_FLOAT, true)?);
            }
            frames.push(Frame { color, luma, t: 0, id: 0, pyr_ok: false });
        }
        let flow_tex = |lw, lh| Tex::new(gpu, lw, lh, DXGI_FORMAT_R16G16B16A16_FLOAT, true);
        let (fw, fh) = (level_size(w, final_level), level_size(h, final_level));
        let mut flows = Vec::new();
        for _ in 0..3 {
            flows.push(PairFlow { fwd: flow_tex(fw, fh)?, bwd: flow_tex(fw, fh)?, ok: false });
        }
        let mut scratch = Vec::new();
        for l in 0..=coarsest {
            if l < final_level {
                scratch.push(None);
            } else {
                let (lw, lh) = (level_size(w, l), level_size(h, l));
                scratch.push(Some(LevelScratch {
                    raw_f: flow_tex(lw, lh)?,
                    raw_b: flow_tex(lw, lh)?,
                    filt_f: flow_tex(lw, lh)?,
                    filt_b: flow_tex(lw, lh)?,
                }));
            }
        }

        let mut vs = None;
        let mut ps = None;
        unsafe {
            dev.CreateVertexShader(&bc.vs, None, Some(&mut vs))?;
            dev.CreatePixelShader(&bc.ps, None, Some(&mut ps))?;
        }

        crate::log!(
            "framegen {}x{}: flow level {} ({}x{}), coarsest level {} ({}x{})",
            w,
            h,
            final_level,
            fw,
            fh,
            coarsest,
            level_size(w, coarsest),
            level_size(h, coarsest)
        );

        Ok(FrameGen {
            ctx: gpu.ctx.clone(),
            cs_pyr_color: gpu.compute_shader(&bc.pyr_color)?,
            cs_pyr_luma: gpu.compute_shader(&bc.pyr_luma)?,
            cs_flow: gpu.compute_shader(&bc.flow)?,
            cs_median: gpu.compute_shader(&bc.median)?,
            vs: vs.unwrap(),
            ps: ps.unwrap(),
            sampler: gpu.linear_clamp_sampler()?,
            cb: ConstBuf::new(gpu)?,
            w,
            h,
            final_level,
            coarsest,
            frames,
            flows,
            scratch,
            newest: None,
            count: 0,
            next_id: 0,
            params: Params::default(),
            profiler: None,
        })
    }

    pub fn reset(&mut self) {
        self.newest = None;
        self.count = 0;
        for f in &mut self.frames {
            f.pyr_ok = false;
        }
        for p in &mut self.flows {
            p.ok = false;
        }
    }

    pub fn frame_count(&self) -> u64 {
        self.count
    }

    fn mark(&mut self, label: &'static str) {
        if let Some(p) = self.profiler.as_mut() {
            p.mark(&self.ctx, label);
        }
    }

    /// Adds a captured frame (optionally cropped by `src_box`) taken at `t` (100 ns).
    pub fn push_frame(&mut self, src: &ID3D11Texture2D, src_box: Option<&D3D11_BOX>, t: i64) -> Result<()> {
        self.push(src, src_box, t, true)
    }

    /// Like `push_frame`, but only stores the image (no motion analysis). Used
    /// while the overlay is hidden: ~0.1 ms instead of ~0.5 ms, and the newest
    /// real frame is still ready the moment the overlay comes back.
    pub fn push_frame_copy_only(&mut self, src: &ID3D11Texture2D, src_box: Option<&D3D11_BOX>, t: i64) -> Result<()> {
        self.push(src, src_box, t, false)
    }

    fn push(&mut self, src: &ID3D11Texture2D, src_box: Option<&D3D11_BOX>, t: i64, analyse: bool) -> Result<()> {
        let slot = self.newest.map(|n| (n + 1) % 3).unwrap_or(0);
        unsafe {
            self.ctx.CopySubresourceRegion(
                &self.frames[slot].color.tex,
                0,
                0,
                0,
                0,
                src,
                0,
                src_box.map(|b| b as *const _),
            );
        }
        self.frames[slot].pyr_ok = false;
        self.flows[slot].ok = false;
        self.mark("copy");

        if analyse {
            self.build_pyramid(slot)?;
            self.frames[slot].pyr_ok = true;
            self.mark("pyramid");
            if let Some(prev) = self.newest {
                if !self.frames[prev].pyr_ok {
                    self.build_pyramid(prev)?;
                    self.frames[prev].pyr_ok = true;
                }
                // The previous pair's flow predicts this one (if it is valid).
                let prior = self.flows[prev].ok.then_some(prev);
                self.compute_flow(prev, slot, prior)?;
                self.flows[slot].ok = true;
            }
        }
        self.count += 1;
        self.next_id += 1;
        self.frames[slot].t = t;
        self.frames[slot].id = self.next_id;
        self.newest = Some(slot);
        Ok(())
    }

    fn bind_cs(
        &self,
        cs: &ID3D11ComputeShader,
        srvs: &[Option<ID3D11ShaderResourceView>],
        uavs: &[Option<ID3D11UnorderedAccessView>],
    ) {
        unsafe {
            self.ctx.CSSetShader(cs, None);
            self.ctx.CSSetShaderResources(0, Some(srvs));
            self.ctx.CSSetUnorderedAccessViews(0, uavs.len() as u32, Some(uavs.as_ptr()), None);
            self.ctx.CSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            self.ctx.CSSetConstantBuffers(0, Some(&[Some(self.cb.buf.clone())]));
        }
    }

    fn unbind_cs(&self) {
        unsafe {
            let none: [Option<ID3D11ShaderResourceView>; 6] = Default::default();
            self.ctx.CSSetShaderResources(0, Some(&none));
            let nu: [Option<ID3D11UnorderedAccessView>; 2] = [None, None];
            self.ctx.CSSetUnorderedAccessViews(0, 2, Some(nu.as_ptr()), None);
        }
    }

    fn dispatch(&self, w: u32, h: u32, z: u32) {
        unsafe { self.ctx.Dispatch(w.div_ceil(8), h.div_ceil(8), z) };
    }

    fn build_pyramid(&self, slot: usize) -> Result<()> {
        let f = &self.frames[slot];
        for (i, dst) in f.luma.iter().enumerate() {
            let (src_w, src_h) = if i == 0 { (f.color.w, f.color.h) } else { (f.luma[i - 1].w, f.luma[i - 1].h) };
            self.cb.write(
                &self.ctx,
                &Consts::default().i(dst.w as i32).i(dst.h as i32).f(1.0 / src_w as f32).f(1.0 / src_h as f32).0,
            )?;
            let uav = [dst.uav.clone()];
            if i == 0 {
                self.bind_cs(&self.cs_pyr_color, &[Some(f.color.srv.clone()), None], &uav);
            } else {
                self.bind_cs(&self.cs_pyr_luma, &[None, Some(f.luma[i - 1].srv.clone())], &uav);
            }
            self.dispatch(dst.w, dst.h, 1);
            self.unbind_cs();
        }
        Ok(())
    }

    /// Forward (a -> b) and backward (b -> a) flow, written to flows[b].
    fn compute_flow(&mut self, a: usize, b: usize, prior_slot: Option<usize>) -> Result<()> {
        let p = self.params;
        for level in (self.final_level..=self.coarsest).rev() {
            let li = level as usize;
            {
                let sc = self.scratch[li].as_ref().unwrap();
                let parent: [Option<ID3D11ShaderResourceView>; 2] = if level < self.coarsest {
                    let ps = self.scratch[li + 1].as_ref().unwrap();
                    if p.median_all_levels {
                        [Some(ps.filt_f.srv.clone()), Some(ps.filt_b.srv.clone())]
                    } else {
                        [Some(ps.raw_f.srv.clone()), Some(ps.raw_b.srv.clone())]
                    }
                } else {
                    [None, None]
                };
                let prior: [Option<ID3D11ShaderResourceView>; 2] = match prior_slot {
                    Some(s) => [Some(self.flows[s].fwd.srv.clone()), Some(self.flows[s].bwd.srv.clone())],
                    None => [None, None],
                };
                let la = &self.frames[a].luma[li - 1];
                let lb = &self.frames[b].luma[li - 1];
                self.cb.write(
                    &self.ctx,
                    &Consts::default()
                        .i(sc.raw_f.w as i32)
                        .i(sc.raw_f.h as i32)
                        .f(1.0 / sc.raw_f.w as f32)
                        .f(1.0 / sc.raw_f.h as f32)
                        .f((1u32 << level) as f32)
                        .f(if parent[0].is_some() { 1.0 } else { 0.0 })
                        .f(if prior[0].is_some() { 1.0 } else { 0.0 })
                        .f(if level == self.coarsest { p.search_radius } else { 0.0 })
                        .f(p.lambda)
                        .f(p.zero_bias)
                        .i(p.iters)
                        .0,
                )?;
                let [pf, pb] = parent;
                let [rf, rb] = prior;
                self.bind_cs(
                    &self.cs_flow,
                    &[Some(la.srv.clone()), Some(lb.srv.clone()), pf, pb, rf, rb],
                    &[sc.raw_f.uav.clone(), sc.raw_b.uav.clone()],
                );
                self.dispatch(sc.raw_f.w, sc.raw_f.h, 2);
                self.unbind_cs();

                let is_final = level == self.final_level;
                if is_final || p.median_all_levels {
                    let (of, ob) =
                        if is_final { (&self.flows[b].fwd, &self.flows[b].bwd) } else { (&sc.filt_f, &sc.filt_b) };
                    self.cb.write(&self.ctx, &Consts::default().i(sc.raw_f.w as i32).i(sc.raw_f.h as i32).0)?;
                    self.bind_cs(
                        &self.cs_median,
                        &[Some(sc.raw_f.srv.clone()), Some(sc.raw_b.srv.clone())],
                        &[of.uav.clone(), ob.uav.clone()],
                    );
                    self.dispatch(sc.raw_f.w, sc.raw_f.h, 2);
                    self.unbind_cs();
                }
            }
            self.mark(match level - self.final_level {
                0 => "flow L0 (final)",
                1 => "flow L1",
                2 => "flow L2",
                3 => "flow L3",
                _ => "flow L4+",
            });
        }
        Ok(())
    }

    /// Slots ordered oldest -> newest.
    fn ordered(&self) -> Vec<usize> {
        let Some(n) = self.newest else { return vec![] };
        let k = self.count.min(3) as usize;
        (0..k).rev().map(|i| (n + 3 - i) % 3).collect()
    }

    /// Timestamp (100 ns) of the newest frame.
    pub fn newest_time(&self) -> Option<i64> {
        self.newest.map(|n| self.frames[n].t)
    }

    /// Renders the image for presentation time `tau` (in the capture clock).
    /// Pairs further apart than `max_gap` (a hitch / loading screen) are not
    /// interpolated; the nearest real frame is shown instead.
    pub fn render(
        &self,
        tau: i64,
        max_gap: i64,
        rtv: &ID3D11RenderTargetView,
        rt_tex: &ID3D11Texture2D,
    ) -> Result<Option<Shown>> {
        let slots = self.ordered();
        if slots.is_empty() {
            return Ok(None);
        }
        let newest = *slots.last().unwrap();
        if slots.len() == 1 || tau >= self.frames[newest].t {
            return self.show_real(newest, rt_tex).map(Some);
        }
        for i in (1..slots.len()).rev() {
            let (a, b) = (slots[i - 1], slots[i]);
            let (ta, tb) = (self.frames[a].t, self.frames[b].t);
            if tau >= ta {
                // Hitch / loading screen, or no motion analysis for this pair
                // (overlay just came back): show the nearest real frame.
                if tb - ta > max_gap || !self.flows[b].ok {
                    let nearest = if tau - ta < tb - tau { a } else { b };
                    return self.show_real(nearest, rt_tex).map(Some);
                }
                let phase = ((tau - ta) as f64 / (tb - ta).max(1) as f64) as f32;
                return self.render_pair(a, b, phase, rtv, rt_tex).map(Some);
            }
        }
        self.show_real(slots[0], rt_tex).map(Some)
    }

    fn show_real(&self, slot: usize, rt_tex: &ID3D11Texture2D) -> Result<Shown> {
        unsafe { self.ctx.CopyResource(rt_tex, &self.frames[slot].color.tex) };
        Ok(Shown::Real(self.frames[slot].id))
    }

    /// Synthesises the frame at `phase` between slot `a` and its successor `b`.
    fn render_pair(
        &self,
        a: usize,
        b: usize,
        phase: f32,
        rtv: &ID3D11RenderTargetView,
        rt_tex: &ID3D11Texture2D,
    ) -> Result<Shown> {
        let phase = phase.clamp(0.0, 1.0);
        if phase < 0.02 {
            return self.show_real(a, rt_tex);
        }
        if phase > 0.98 {
            return self.show_real(b, rt_tex);
        }
        let fa = &self.frames[a];
        let fb = &self.frames[b];
        let flow = &self.flows[b];
        let p = self.params;
        self.cb.write(
            &self.ctx,
            &Consts::default()
                .f(self.w as f32)
                .f(self.h as f32)
                .f(1.0 / self.w as f32)
                .f(1.0 / self.h as f32)
                .f(phase)
                .f(p.sharp)
                .f(p.occ_lo)
                .f(p.occ_hi)
                .f(p.debug_tint)
                .0,
        )?;
        unsafe {
            let ctx = &self.ctx;
            let vp = D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: self.w as f32,
                Height: self.h as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            ctx.RSSetViewports(Some(&[vp]));
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.IASetInputLayout(None);
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetShader(&self.ps, None);
            ctx.PSSetShaderResources(
                0,
                Some(&[
                    Some(fa.color.srv.clone()),
                    Some(fb.color.srv.clone()),
                    Some(flow.fwd.srv.clone()),
                    Some(flow.bwd.srv.clone()),
                ]),
            );
            ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            ctx.PSSetConstantBuffers(0, Some(&[Some(self.cb.buf.clone())]));
            ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            ctx.Draw(3, 0);
            let none: [Option<ID3D11ShaderResourceView>; 6] = Default::default();
            ctx.PSSetShaderResources(0, Some(&none));
            ctx.OMSetRenderTargets(None, None);
        }
        Ok(Shown::Generated { a: fa.id, b: fb.id, phase_q: (phase * 1000.0) as u32 })
    }

    /// Test helper: render the pair (second newest -> newest) at an exact phase.
    pub fn render_latest_pair(
        &self,
        phase: f32,
        rtv: &ID3D11RenderTargetView,
        rt_tex: &ID3D11Texture2D,
    ) -> Result<Shown> {
        let slots = self.ordered();
        let n = slots.len();
        assert!(n >= 2, "need two frames");
        self.render_pair(slots[n - 2], slots[n - 1], phase, rtv, rt_tex)
    }
}

#[cfg(test)]
mod tests {
    use super::level_size;

    #[test]
    fn pyramid_sizes() {
        assert_eq!(level_size(2560, 2), 640);
        assert_eq!(level_size(1440, 5), 45);
        assert_eq!(level_size(1081, 1), 541, "odd sizes round up");
        assert_eq!(level_size(3, 10), 1, "never zero");
    }
}
