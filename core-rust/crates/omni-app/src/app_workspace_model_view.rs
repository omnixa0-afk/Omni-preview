//! A model's look without its program (docs/decisions/workspace-files-storage.md, «3D-просмотр»): an
//! STL mesh — binary or text — drawn here in the core, the same on every platform. Orthographic, Z up
//! as in CAD, turned by `yaw` about Z and tilted by `pitch`; lit along the view, both sides of a face
//! alike (STL normals are often wrong), on a transparent background that suits both themes.

use omni_types::{OmniError, OmniResult};

/// One triangle's three corners.
pub(super) type Triangle = [[f32; 3]; 3];

/// The most triangles drawn (a mesh above it is refused, not cut).
const TRIANGLES_MAX: usize = 4_000_000;
/// The largest picture asked for, in pixels a side.
pub(super) const VIEW_MAX_SIDE: u32 = 1600;

fn bad(what: &str) -> OmniError {
    OmniError::InvalidInput(format!("not an STL model: {what}"))
}

/// The triangles of an STL file: binary when its size is that of its count, text otherwise.
pub(super) fn parse_stl(bytes: &[u8]) -> OmniResult<Vec<Triangle>> {
    if bytes.len() >= 84 {
        let count = u32::from_le_bytes([bytes[80], bytes[81], bytes[82], bytes[83]]) as usize;
        if count.checked_mul(50).and_then(|n| n.checked_add(84)) == Some(bytes.len()) {
            if count > TRIANGLES_MAX {
                return Err(bad("too many triangles"));
            }
            let float = |at: usize| f32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
            return Ok((0..count)
                .map(|i| {
                    let at = 84 + i * 50 + 12;
                    let corner = |c: usize| [float(at + c * 12), float(at + c * 12 + 4), float(at + c * 12 + 8)];
                    [corner(0), corner(1), corner(2)]
                })
                .collect());
        }
    }
    let text = std::str::from_utf8(bytes).map_err(|_| bad("neither binary nor text"))?;
    if !text.trim_start().starts_with("solid") {
        return Err(bad("no «solid» at the start"));
    }
    let mut corners: Vec<[f32; 3]> = Vec::new();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        if words.next() != Some("vertex") {
            continue;
        }
        let mut corner = [0f32; 3];
        for value in &mut corner {
            *value = words.next().and_then(|w| w.parse().ok()).ok_or_else(|| bad("a vertex without three numbers"))?;
        }
        corners.push(corner);
        if corners.len() / 3 > TRIANGLES_MAX {
            return Err(bad("too many triangles"));
        }
    }
    if corners.is_empty() || !corners.len().is_multiple_of(3) {
        return Err(bad("its vertices do not make whole triangles"));
    }
    Ok(corners.as_chunks::<3>().0.to_vec())
}

/// The model as a PNG of `width`×`height`, turned by `yaw` degrees about Z and tilted by `pitch`.
pub(super) fn render_png(triangles: &[Triangle], width: u32, height: u32, yaw: f32, pitch: f32) -> OmniResult<Vec<u8>> {
    let (width, height) = (width.clamp(16, VIEW_MAX_SIDE), height.clamp(16, VIEW_MAX_SIDE));
    let finite: Vec<&Triangle> = triangles.iter().filter(|t| t.iter().flatten().all(|v| v.is_finite())).collect();
    if finite.is_empty() {
        return Err(bad("no triangles"));
    }
    // Centre and size: the box around every corner; the model fills ~90% of the smaller side.
    let (mut low, mut high) = ([f32::MAX; 3], [f32::MIN; 3]);
    for corner in finite.iter().flat_map(|t| t.iter()) {
        for axis in 0..3 {
            low[axis] = low[axis].min(corner[axis]);
            high[axis] = high[axis].max(corner[axis]);
        }
    }
    let centre = [(low[0] + high[0]) / 2.0, (low[1] + high[1]) / 2.0, (low[2] + high[2]) / 2.0];
    let radius = ((high[0] - low[0]).powi(2) + (high[1] - low[1]).powi(2) + (high[2] - low[2]).powi(2)).sqrt() / 2.0;
    let scale = if radius > 0.0 { 0.9 * width.min(height) as f32 / (2.0 * radius) } else { 1.0 };
    let (sy, cy) = yaw.to_radians().sin_cos();
    let (sp, cp) = pitch.to_radians().sin_cos();
    // Screen x to the right, y up, depth towards the viewer.
    let turn = |v: &[f32; 3]| {
        let (x, y, z) = (v[0] - centre[0], v[1] - centre[1], v[2] - centre[2]);
        let (x, y) = (x * cy - y * sy, x * sy + y * cy);
        let (y, z) = (y * cp - z * sp, y * sp + z * cp);
        // Looking along +y: the viewer stands at -y.
        [x, z, -y]
    };
    let (w, h) = (width as usize, height as usize);
    let mut depth = vec![f32::MIN; w * h];
    let mut pixels = vec![0u8; w * h * 4];
    for triangle in finite {
        let [a, b, c] = [turn(&triangle[0]), turn(&triangle[1]), turn(&triangle[2])];
        let normal = cross(sub(b, a), sub(c, a));
        let length = (normal[0] * normal[0] + normal[1] * normal[1] + normal[2] * normal[2]).sqrt();
        if length == 0.0 {
            continue;
        }
        // Lit from the viewer and a little above: both sides alike.
        let light = (normal[2] * 0.85 + normal[1] * 0.45).abs() / length;
        let shade = 0.28 + 0.72 * light.min(1.0);
        let colour = [(150.0 * shade + 30.0) as u8, (165.0 * shade + 30.0) as u8, (190.0 * shade + 35.0) as u8];
        let screen = |v: [f32; 3]| (w as f32 / 2.0 + v[0] * scale, h as f32 / 2.0 - v[1] * scale, v[2]);
        let (p, q, r) = (screen(a), screen(b), screen(c));
        let area = (q.0 - p.0) * (r.1 - p.1) - (q.1 - p.1) * (r.0 - p.0);
        if area.abs() < 1e-6 {
            continue;
        }
        let x0 = p.0.min(q.0).min(r.0).floor().max(0.0) as usize;
        let x1 = (p.0.max(q.0).max(r.0).ceil() as isize).clamp(0, w as isize - 1) as usize;
        let y0 = p.1.min(q.1).min(r.1).floor().max(0.0) as usize;
        let y1 = (p.1.max(q.1).max(r.1).ceil() as isize).clamp(0, h as isize - 1) as usize;
        for y in y0..=y1 {
            for x in x0..=x1 {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                let wa = ((q.0 - px) * (r.1 - py) - (q.1 - py) * (r.0 - px)) / area;
                let wb = ((r.0 - px) * (p.1 - py) - (r.1 - py) * (p.0 - px)) / area;
                let wc = 1.0 - wa - wb;
                if wa < 0.0 || wb < 0.0 || wc < 0.0 {
                    continue;
                }
                let z = wa * p.2 + wb * q.2 + wc * r.2;
                let at = y * w + x;
                if z > depth[at] {
                    depth[at] = z;
                    pixels[at * 4..at * 4 + 4].copy_from_slice(&[colour[0], colour[1], colour[2], 255]);
                }
            }
        }
    }
    png(width, height, &pixels)
}

/// RGBA pixels as a PNG: one zlib stream of the rows, each led by filter byte 0. Written here, as
/// the core has flate2 already and no image library outside tests.
fn png(width: u32, height: u32, rgba: &[u8]) -> OmniResult<Vec<u8>> {
    use std::io::Write as _;
    let row = width as usize * 4;
    let mut raw = Vec::with_capacity((row + 1) * height as usize);
    for line in rgba.chunks_exact(row) {
        raw.push(0);
        raw.extend_from_slice(line);
    }
    let mut zlib = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    zlib.write_all(&raw).map_err(|e| OmniError::Internal(format!("png: {e}")))?;
    let data = zlib.finish().map_err(|e| OmniError::Internal(format!("png: {e}")))?;
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::with_capacity(13);
    header.extend(width.to_be_bytes());
    header.extend(height.to_be_bytes());
    // 8 bits a channel, RGBA, deflate, adaptive filtering, no interlace.
    header.extend([8, 6, 0, 0, 0]);
    for (kind, body) in [(b"IHDR", header.as_slice()), (b"IDAT", data.as_slice()), (b"IEND", &[][..])] {
        out.extend((body.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        let crc = crc32(&out[start..]);
        out.extend(crc.to_be_bytes());
    }
    Ok(out)
}

/// CRC-32 (IEEE) as PNG chunks carry it.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unit cube's 12 triangles as binary STL.
    fn cube() -> Vec<u8> {
        let v = |x: f32, y: f32, z: f32| [x, y, z];
        let faces: Vec<Triangle> = vec![
            [v(0., 0., 0.), v(1., 0., 0.), v(1., 1., 0.)], [v(0., 0., 0.), v(1., 1., 0.), v(0., 1., 0.)],
            [v(0., 0., 1.), v(1., 0., 1.), v(1., 1., 1.)], [v(0., 0., 1.), v(1., 1., 1.), v(0., 1., 1.)],
            [v(0., 0., 0.), v(1., 0., 0.), v(1., 0., 1.)], [v(0., 0., 0.), v(1., 0., 1.), v(0., 0., 1.)],
            [v(0., 1., 0.), v(1., 1., 0.), v(1., 1., 1.)], [v(0., 1., 0.), v(1., 1., 1.), v(0., 1., 1.)],
            [v(0., 0., 0.), v(0., 1., 0.), v(0., 1., 1.)], [v(0., 0., 0.), v(0., 1., 1.), v(0., 0., 1.)],
            [v(1., 0., 0.), v(1., 1., 0.), v(1., 1., 1.)], [v(1., 0., 0.), v(1., 1., 1.), v(1., 0., 1.)],
        ];
        let mut bytes = vec![0u8; 80];
        bytes.extend((faces.len() as u32).to_le_bytes());
        for face in &faces {
            bytes.extend([0u8; 12]);
            for corner in face {
                for value in corner {
                    bytes.extend(value.to_le_bytes());
                }
            }
            bytes.extend([0u8; 2]);
        }
        bytes
    }

    /// To look at a real model: OMNI_STL_IN=<file.stl> OMNI_STL_OUT=<picture.png>.
    #[test]
    #[ignore = "draws a model named by the environment, for a person to look at"]
    fn draw_a_model_to_look_at() {
        let (Ok(input), Ok(output)) = (std::env::var("OMNI_STL_IN"), std::env::var("OMNI_STL_OUT")) else { return };
        let triangles = parse_stl(&std::fs::read(input).unwrap()).unwrap();
        std::fs::write(output, render_png(&triangles, 480, 360, 35.0, 25.0).unwrap()).unwrap();
    }

    #[test]
    fn a_binary_and_a_text_stl_read_alike_and_draw_into_the_middle() {
        let binary = parse_stl(&cube()).unwrap();
        assert_eq!(binary.len(), 12);
        let text = "solid c\n facet normal 0 0 1\n  outer loop\n   vertex 0 0 0\n   vertex 1 0 0\n   vertex 1 1 0\n  endloop\n endfacet\nendsolid c\n";
        assert_eq!(parse_stl(text.as_bytes()).unwrap(), vec![binary[0]]);
        assert!(parse_stl(b"not a model").is_err());
        let png = render_png(&binary, 64, 48, 30.0, 25.0).unwrap();
        let picture = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!((picture.width(), picture.height()), (64, 48));
        assert_eq!(picture.get_pixel(32, 24)[3], 255, "the cube covers the middle");
        assert_eq!(picture.get_pixel(0, 0)[3], 0, "the corner stays transparent");
    }
}
