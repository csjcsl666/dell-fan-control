//! The application icon, drawn in code so it needs no image files at runtime
//!
//! A rounded square with a cool blue gradient and a white four blade fan. The
//! same shape is shipped as `packaging/dell-fan-control.svg` for launchers.
//! Original artwork for this project, no third party marks

use std::f32::consts::PI;

/// Coverage of the fan glyph at a point, coordinates in -1..1
fn fan_shape(x: f32, y: f32, spin: f32) -> bool {
    let r = (x * x + y * y).sqrt();
    if r < 0.17 {
        return true; // hub
    }
    if !(0.17..=0.80).contains(&r) {
        return false;
    }
    let a = y.atan2(x);
    for k in 0..4 {
        // each blade sweeps a little with radius, which gives the curved look
        let center = spin + k as f32 * PI / 2.0 + 0.9 * (r - 0.17);
        let mut d = (a - center).rem_euclid(2.0 * PI);
        if d > PI {
            d -= 2.0 * PI;
        }
        let half = 0.42 * (1.0 - 0.45 * (r - 0.17) / 0.63);
        if d.abs() < half && d > -half * 0.55 {
            return true;
        }
    }
    false
}

fn rounded_square(x: f32, y: f32) -> bool {
    let m = 0.78;
    let rad = 0.22;
    let qx = (x.abs() - m).max(0.0);
    let qy = (y.abs() - m).max(0.0);
    x.abs() <= m + rad && y.abs() <= m + rad && (qx * qx + qy * qy).sqrt() <= rad
}

/// RGBA pixels, `size` x `size`, 4x4 supersampled
pub fn rgba(size: u32) -> Vec<u8> {
    let mut out = vec![0u8; (size * size * 4) as usize];
    let s = size as f32;
    for py in 0..size {
        for px in 0..size {
            let mut bg = 0.0;
            let mut fg = 0.0;
            for sy in 0..4 {
                for sx in 0..4 {
                    let x = ((px as f32 + (sx as f32 + 0.5) / 4.0) / s) * 2.0 - 1.0;
                    let y = ((py as f32 + (sy as f32 + 0.5) / 4.0) / s) * 2.0 - 1.0;
                    if rounded_square(x, y) {
                        bg += 1.0;
                        if fan_shape(x, y, 0.35) {
                            fg += 1.0;
                        }
                    }
                }
            }
            bg /= 16.0;
            fg /= 16.0;
            let t = py as f32 / s;
            // gradient from cyan at the top to deep blue at the bottom
            let (r0, g0, b0) = (34.0, 196.0, 230.0);
            let (r1, g1, b1) = (37.0, 99.0, 235.0);
            let mix = |a: f32, b: f32| a + (b - a) * t;
            let (br, bgc, bb) = (mix(r0, r1), mix(g0, g1), mix(b0, b1));
            let w = if bg > 0.0 { fg / bg } else { 0.0 };
            let i = ((py * size + px) * 4) as usize;
            out[i] = (br + (255.0 - br) * w) as u8;
            out[i + 1] = (bgc + (255.0 - bgc) * w) as u8;
            out[i + 2] = (bb + (255.0 - bb) * w) as u8;
            out[i + 3] = (bg * 255.0) as u8;
        }
    }
    out
}

/// ARGB32 in network byte order, as StatusNotifierItem expects
pub fn argb(size: u32) -> Vec<u8> {
    let mut v = rgba(size);
    for px in v.as_chunks_mut::<4>().0 {
        let (r, g, b, a) = (px[0], px[1], px[2], px[3]);
        px.copy_from_slice(&[a, r, g, b]);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_has_opaque_body_and_transparent_corners() {
        let s = 64;
        let p = rgba(s);
        let at = |x: u32, y: u32| p[((y * s + x) * 4 + 3) as usize];
        assert_eq!(at(0, 0), 0);
        assert_eq!(at(32, 32), 255);
        // the hub is white
        let i = ((32 * s + 32) * 4) as usize;
        assert!(p[i] > 240 && p[i + 1] > 240 && p[i + 2] > 240);
    }
}
