//! Dependency-free rasteriser for the app icon (shared by the app and build.rs):
//! three stacked frames on a blue-violet gradient. `active` adds a green dot.

fn round_rect_cov(x: f32, y: f32, l: f32, t: f32, r: f32, b: f32, rad: f32) -> bool {
    if x < l || x > r || y < t || y > b {
        return false;
    }
    let cx = x.clamp(l + rad, r - rad);
    let cy = y.clamp(t + rad, b - rad);
    let (dx, dy) = (x - cx, y - cy);
    dx * dx + dy * dy <= rad * rad
}

/// Straight-alpha BGRA pixels, top-down, `size` x `size`.
pub fn rasterize(size: u32, active: bool) -> Vec<u8> {
    let s = size as f32;
    let ss = 4; // 4x4 supersampling
    let mut out = vec![0u8; (size * size * 4) as usize];
    for py in 0..size {
        for px in 0..size {
            let mut acc = [0.0f32; 4]; // premultiplied rgba
            for sy in 0..ss {
                for sx in 0..ss {
                    let x = (px as f32 + (sx as f32 + 0.5) / ss as f32) / s;
                    let y = (py as f32 + (sy as f32 + 0.5) / ss as f32) / s;
                    let mut c = [0.0f32; 4];
                    if round_rect_cov(x, y, 0.04, 0.04, 0.96, 0.96, 0.22) {
                        let g = ((x + y) * 0.5).clamp(0.0, 1.0);
                        c = [0.31 + (0.49 - 0.31) * g, 0.49 + (0.36 - 0.49) * g, 1.0, 1.0];
                        // stacked frames, back to front
                        let frames = [
                            (0.36, 0.20, 0.82, 0.56, 0.35),
                            (0.27, 0.30, 0.73, 0.66, 0.62),
                            (0.18, 0.40, 0.64, 0.76, 1.0),
                        ];
                        for (l, t, r, b, a) in frames {
                            if round_rect_cov(x, y, l, t, r, b, 0.06) {
                                for ch in c.iter_mut().take(3) {
                                    *ch = *ch * (1.0 - a) + a;
                                }
                            }
                        }
                    }
                    if active {
                        let (dx, dy) = (x - 0.80, y - 0.80);
                        let d2 = dx * dx + dy * dy;
                        if d2 < 0.20 * 0.20 {
                            c = if d2 < 0.15 * 0.15 { [0.13, 0.77, 0.37, 1.0] } else { [0.06, 0.07, 0.08, 1.0] };
                        }
                    }
                    for k in 0..3 {
                        acc[k] += c[k] * c[3];
                    }
                    acc[3] += c[3];
                }
            }
            let n = (ss * ss) as f32;
            let a = acc[3] / n;
            let o = ((py * size + px) * 4) as usize;
            if a > 0.0 {
                out[o] = ((acc[2] / n / a) * 255.0).round().min(255.0) as u8;
                out[o + 1] = ((acc[1] / n / a) * 255.0).round().min(255.0) as u8;
                out[o + 2] = ((acc[0] / n / a) * 255.0).round().min(255.0) as u8;
                out[o + 3] = (a * 255.0).round() as u8;
            }
        }
    }
    out
}

/// A multi-resolution .ico file (32-bit BMP entries) for Explorer / the exe.
pub fn ico_file(sizes: &[u32]) -> Vec<u8> {
    let mut images: Vec<Vec<u8>> = Vec::new();
    for &s in sizes {
        let px = rasterize(s, false);
        let mut img = Vec::new();
        // BITMAPINFOHEADER, height doubled (XOR image + AND mask)
        img.extend_from_slice(&40u32.to_le_bytes());
        img.extend_from_slice(&(s as i32).to_le_bytes());
        img.extend_from_slice(&((s * 2) as i32).to_le_bytes());
        img.extend_from_slice(&1u16.to_le_bytes());
        img.extend_from_slice(&32u16.to_le_bytes());
        img.extend_from_slice(&[0u8; 24]);
        for y in (0..s).rev() {
            let row = (y * s * 4) as usize;
            img.extend_from_slice(&px[row..row + (s * 4) as usize]);
        }
        let mask_row = (s.div_ceil(32) * 4) as usize;
        img.extend(std::iter::repeat_n(0u8, mask_row * s as usize));
        images.push(img);
    }
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (i, &s) in sizes.iter().enumerate() {
        let dim = if s >= 256 { 0u8 } else { s as u8 };
        out.extend_from_slice(&[dim, dim, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&(images[i].len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += images[i].len() as u32;
    }
    for img in images {
        out.extend_from_slice(&img);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raster_has_opaque_body_and_transparent_corner() {
        let px = rasterize(32, false);
        assert_eq!(px.len(), 32 * 32 * 4);
        assert_eq!(px[3], 0, "top-left corner is transparent (rounded)");
        let mid = ((16 * 32 + 16) * 4) as usize;
        assert_eq!(px[mid + 3], 255, "centre is opaque");
    }

    #[test]
    fn ico_directory_is_consistent() {
        let sizes = [16u32, 32, 128];
        let ico = ico_file(&sizes);
        assert_eq!(u16::from_le_bytes([ico[2], ico[3]]), 1, "type = icon");
        assert_eq!(u16::from_le_bytes([ico[4], ico[5]]) as usize, sizes.len());
        let mut end = 0usize;
        for (i, &size) in sizes.iter().enumerate() {
            let e = 6 + 16 * i;
            let len = u32::from_le_bytes(ico[e + 8..e + 12].try_into().unwrap()) as usize;
            let off = u32::from_le_bytes(ico[e + 12..e + 16].try_into().unwrap()) as usize;
            assert_eq!(ico[e] as u32, size);
            end = end.max(off + len);
        }
        assert_eq!(end, ico.len(), "entries cover the file exactly");
    }
}
