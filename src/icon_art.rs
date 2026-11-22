// Procedural DataVivid icon, shared by build.rs (Windows .ico resource) and the
// app (window icon), so the repository needs no binary image assets.
//
// The design: a rounded square with a teal-to-violet gradient and a white
// table grid whose header row and one highlighted cell are solid.

/// Returns `size * size` RGBA pixels (row-major, top row first).
pub fn icon_rgba(size: u32) -> Vec<u8> {
    const SS: u32 = 4; // supersampling factor per axis
    let mut out = vec![0u8; (size * size * 4) as usize];
    let s = size as f32;
    for y in 0..size {
        for x in 0..size {
            let mut acc = [0f32; 4];
            for sy in 0..SS {
                for sx in 0..SS {
                    let u = (x as f32 + (sx as f32 + 0.5) / SS as f32) / s;
                    let v = (y as f32 + (sy as f32 + 0.5) / SS as f32) / s;
                    let c = sample(u, v);
                    // premultiplied accumulation
                    acc[0] += c[0] * c[3];
                    acc[1] += c[1] * c[3];
                    acc[2] += c[2] * c[3];
                    acc[3] += c[3];
                }
            }
            let n = (SS * SS) as f32;
            let a = acc[3] / n;
            let i = ((y * size + x) * 4) as usize;
            if a > 0.0 {
                out[i] = (acc[0] / acc[3] * 255.0).round().clamp(0.0, 255.0) as u8;
                out[i + 1] = (acc[1] / acc[3] * 255.0).round().clamp(0.0, 255.0) as u8;
                out[i + 2] = (acc[2] / acc[3] * 255.0).round().clamp(0.0, 255.0) as u8;
            }
            out[i + 3] = (a * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

/// Colour (r, g, b, a in 0..1) at normalised coordinates (u, v).
fn sample(u: f32, v: f32) -> [f32; 4] {
    let margin = 0.04;
    let radius = 0.22;
    if !in_rounded_rect(u, v, margin, margin, 1.0 - margin, 1.0 - margin, radius) {
        return [0.0, 0.0, 0.0, 0.0];
    }
    // Diagonal gradient: teal (top-left) -> violet (bottom-right).
    let t = ((u + v) * 0.5).clamp(0.0, 1.0);
    let teal = [0.05, 0.71, 0.66];
    let violet = [0.42, 0.25, 0.86];
    let mut c = [
        teal[0] + (violet[0] - teal[0]) * t,
        teal[1] + (violet[1] - teal[1]) * t,
        teal[2] + (violet[2] - teal[2]) * t,
        1.0,
    ];

    // Table grid: 3 columns x 4 rows inside the inner box.
    let (x0, y0, x1, y1) = (0.20, 0.22, 0.80, 0.80);
    let line = 0.035;
    if u >= x0 && u <= x1 && v >= y0 && v <= y1 {
        let gx = (u - x0) / (x1 - x0);
        let gy = (v - y0) / (y1 - y0);
        let header = gy < 0.25;
        let col = ((gx * 3.0) as i32).min(2);
        let row = ((gy * 4.0) as i32).min(3);
        let on_vline = [0.0f32, 1.0 / 3.0, 2.0 / 3.0, 1.0]
            .iter()
            .any(|&p| ((gx - p) * (x1 - x0)).abs() < line / 2.0);
        let on_hline = [0.0f32, 0.25, 0.5, 0.75, 1.0]
            .iter()
            .any(|&p| ((gy - p) * (y1 - y0)).abs() < line / 2.0);
        let highlight = col == 1 && row == 2;
        if header || on_vline || on_hline {
            c = [1.0, 1.0, 1.0, 1.0];
        } else if highlight {
            c = [1.0, 0.80, 0.30, 1.0];
        } else {
            // translucent white cell wash
            c = [
                c[0] + (1.0 - c[0]) * 0.18,
                c[1] + (1.0 - c[1]) * 0.18,
                c[2] + (1.0 - c[2]) * 0.18,
                1.0,
            ];
        }
    }
    c
}

fn in_rounded_rect(u: f32, v: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> bool {
    if u < x0 || u > x1 || v < y0 || v > y1 {
        return false;
    }
    let cx = u.clamp(x0 + r, x1 - r);
    let cy = v.clamp(y0 + r, y1 - r);
    let dx = u - cx;
    let dy = v - cy;
    dx * dx + dy * dy <= r * r
}

/// Encodes the icon as a multi-resolution Windows .ico (32-bit BMP entries).
#[allow(dead_code)]
pub fn ico_bytes(sizes: &[u32]) -> Vec<u8> {
    let images: Vec<(u32, Vec<u8>)> = sizes
        .iter()
        .map(|&s| (s, bmp_entry(s, &icon_rgba(s))))
        .collect();
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&1u16.to_le_bytes()); // type: icon
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (s, data) in &images {
        let dim = if *s >= 256 { 0u8 } else { *s as u8 };
        out.push(dim);
        out.push(dim);
        out.push(0); // palette
        out.push(0); // reserved
        out.extend_from_slice(&1u16.to_le_bytes()); // planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bpp
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += data.len() as u32;
    }
    for (_, data) in images {
        out.extend_from_slice(&data);
    }
    out
}

fn bmp_entry(size: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let and_row = size.div_ceil(32) * 4;
    // BITMAPINFOHEADER
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(size as i32).to_le_bytes());
    out.extend_from_slice(&((size * 2) as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&(size * size * 4 + and_row * size).to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    // XOR bitmap, bottom-up BGRA
    for y in (0..size).rev() {
        for x in 0..size {
            let i = ((y * size + x) * 4) as usize;
            out.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }
    // AND mask (all zero: alpha channel decides transparency)
    out.extend(std::iter::repeat_n(0u8, (and_row * size) as usize));
    out
}
