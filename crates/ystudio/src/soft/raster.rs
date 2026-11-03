//! A CPU rasteriser for egui's tessellated meshes.
//!
//! egui meshes are vertex-coloured, textured triangles with premultiplied
//! alpha. The frame is split into horizontal strips that are rendered in
//! parallel. Each strip is hashed from the triangles that touch it, and a strip
//! whose hash matches the previous frame is not drawn again. Unchanged frames
//! (hover, cursor moves, blinking timers elsewhere) therefore cost very little,
//! and only the changed strips are sent to the window.

use std::collections::HashMap;

use eframe::egui::epaint::{ClippedPrimitive, ImageData, Primitive, Vertex};
use eframe::egui::{Color32, TextureFilter, TextureId, TexturesDelta};
use rayon::prelude::*;

/// Strip height in pixels: the unit of parallelism and of damage tracking.
const STRIP: usize = 16;
/// Sub-pixel precision of the edge functions (4 bits).
const SUB: i64 = 16;

struct Texture {
    w: usize,
    h: usize,
    /// Premultiplied RGBA.
    px: Vec<[u8; 4]>,
    linear: bool,
    /// Bumped on every update so strips using the texture are redrawn.
    generation: u64,
}

impl Texture {
    #[inline]
    fn fetch(&self, x: i32, y: i32) -> [f32; 4] {
        let x = x.clamp(0, self.w as i32 - 1) as usize;
        let y = y.clamp(0, self.h as i32 - 1) as usize;
        self.px[y * self.w + x].map(f32::from)
    }

    /// Samples at texel coordinates (u * width, v * height).
    #[inline]
    fn sample(&self, tu: f32, tv: f32) -> [f32; 4] {
        if !self.linear {
            return self.fetch(tu.floor() as i32, tv.floor() as i32);
        }
        let (x, y) = (tu - 0.5, tv - 0.5);
        let (fx0, fy0) = (x.floor(), y.floor());
        let (fx, fy) = (x - fx0, y - fy0);
        let (x0, y0) = (fx0 as i32, fy0 as i32);
        let a = self.fetch(x0, y0);
        let b = self.fetch(x0 + 1, y0);
        let c = self.fetch(x0, y0 + 1);
        let d = self.fetch(x0 + 1, y0 + 1);
        let mut out = [0.0; 4];
        for i in 0..4 {
            let top = a[i] + (b[i] - a[i]) * fx;
            let bottom = c[i] + (d[i] - c[i]) * fx;
            out[i] = top + (bottom - top) * fy;
        }
        out
    }
}

/// Linear attribute `f(x, y) = a*x + b*y + c` over a triangle, in pixel space.
#[derive(Clone, Copy)]
struct Plane {
    a: f32,
    b: f32,
    c: f32,
}

impl Plane {
    fn new(p: &[[f32; 2]; 3], f: [f32; 3], inv_area: f32) -> Self {
        let (x0, y0) = (p[0][0], p[0][1]);
        let (dx1, dy1) = (p[1][0] - x0, p[1][1] - y0);
        let (dx2, dy2) = (p[2][0] - x0, p[2][1] - y0);
        let (df1, df2) = (f[1] - f[0], f[2] - f[0]);
        let a = (df1 * dy2 - df2 * dy1) * inv_area;
        let b = (df2 * dx1 - df1 * dx2) * inv_area;
        Self { a, b, c: f[0] - a * x0 - b * y0 }
    }

    #[inline]
    fn at(&self, x: f32, y: f32) -> f32 {
        self.a * x + self.b * y + self.c
    }
}

enum Shade {
    /// One premultiplied colour; `opaque` is set when alpha is 255.
    Solid { rgba: [u32; 4], opaque: Option<u32> },
    /// Per-vertex colours, untextured (anti-aliasing feathers, gradients).
    Gouraud([Plane; 4]),
    /// Textured (glyphs, images).
    Textured { tex: usize, color: [Plane; 4], u: Plane, v: Plane },
}

/// A triangle ready to rasterise: edge functions in fixed point, evaluated at
/// pixel centres as `a*px + b*py + c`, inside when all three are >= 0.
struct Tri {
    edges: [[i64; 3]; 3],
    x0: i32,
    x1: i32,
    y0: i32,
    y1: i32,
    shade: Shade,
}

pub struct SoftRenderer {
    textures: HashMap<TextureId, usize>,
    store: Vec<Option<Texture>>,
    next_generation: u64,
    fb: Vec<u32>,
    width: usize,
    height: usize,
    strip_hashes: Vec<u64>,
    tris: Vec<Tri>,
    bins: Vec<Vec<u32>>,
}

pub struct Painted {
    /// Row ranges `[y0, y1)` that changed, merged into runs.
    pub damage: Vec<(usize, usize)>,
}

impl SoftRenderer {
    pub fn new() -> Self {
        Self {
            textures: HashMap::new(),
            store: Vec::new(),
            next_generation: 1,
            fb: Vec::new(),
            width: 0,
            height: 0,
            strip_hashes: Vec::new(),
            tris: Vec::new(),
            bins: Vec::new(),
        }
    }

    /// The frame buffer, `0x00RRGGBB` per pixel, row-major.
    pub fn pixels(&self) -> &[u32] {
        &self.fb
    }

    pub fn set_textures(&mut self, delta: &TexturesDelta) {
        for (id, deltas) in &delta.set {
            for d in deltas {
                let ImageData::Color(image) = &d.image;
                let [w, h] = image.size;
                let linear = d.options.magnification == TextureFilter::Linear;
                let generation = self.next_generation;
                self.next_generation += 1;
                let pixels = image.pixels.iter().map(|c| c.to_array());
                match (d.pos, self.textures.get(id).copied()) {
                    (Some([px, py]), Some(slot)) => {
                        let Some(t) = self.store[slot].as_mut() else { continue };
                        for (i, p) in pixels.enumerate() {
                            let (x, y) = (px + i % w, py + i / w);
                            if x < t.w && y < t.h {
                                t.px[y * t.w + x] = p;
                            }
                        }
                        t.linear = linear;
                        t.generation = generation;
                    }
                    _ => {
                        let tex = Texture { w, h, px: pixels.collect(), linear, generation };
                        let slot = match self.textures.get(id) {
                            Some(&slot) => slot,
                            None => {
                                self.store.push(None);
                                self.store.len() - 1
                            }
                        };
                        self.store[slot] = Some(tex);
                        self.textures.insert(*id, slot);
                    }
                }
            }
        }
    }

    pub fn free_textures(&mut self, delta: &TexturesDelta) {
        for id in &delta.free {
            if let Some(slot) = self.textures.remove(id) {
                self.store[slot] = None;
            }
        }
    }

    /// Rasterises a frame into the internal buffer; returns the rows that changed.
    pub fn paint(&mut self, width: usize, height: usize, ppp: f32, clear: Color32, prims: &[ClippedPrimitive]) -> Painted {
        let resized = width != self.width || height != self.height;
        if resized {
            self.width = width;
            self.height = height;
            self.fb = vec![0; width * height];
        }
        let strips = height.div_ceil(STRIP);
        if resized || self.strip_hashes.len() != strips {
            self.strip_hashes = vec![0; strips];
        }
        self.bins.resize_with(strips, Vec::new);
        for b in &mut self.bins {
            b.clear();
        }

        let clear_u32 = pack(clear.r() as u32, clear.g() as u32, clear.b() as u32);
        let seed = mix(mix(0x9E37_79B9_7F4A_7C15, clear_u32 as u64), ((width as u64) << 32) | height as u64);
        let mut hashes = vec![seed; strips];

        self.tris.clear();
        for prim in prims {
            let Primitive::Mesh(mesh) = &prim.primitive else { continue };
            let clip = [
                (prim.clip_rect.min.x * ppp).round().clamp(0.0, width as f32) as i32,
                (prim.clip_rect.min.y * ppp).round().clamp(0.0, height as f32) as i32,
                (prim.clip_rect.max.x * ppp).round().clamp(0.0, width as f32) as i32,
                (prim.clip_rect.max.y * ppp).round().clamp(0.0, height as f32) as i32,
            ];
            if clip[0] >= clip[2] || clip[1] >= clip[3] {
                continue;
            }
            let tex = self.textures.get(&mesh.texture_id).and_then(|&s| self.store[s].as_ref().map(|t| (s, t)));
            let mesh_hash = [tex.map_or(0, |t| t.1.generation), clip[0] as u64 | ((clip[1] as u64) << 32), clip[2] as u64 | ((clip[3] as u64) << 32)]
                .into_iter()
                .fold(seed, mix);
            for &[a, b, c] in mesh.indices.as_chunks::<3>().0 {
                let v = [&mesh.vertices[a as usize], &mesh.vertices[b as usize], &mesh.vertices[c as usize]];
                let Some(tri) = setup(v, ppp, clip, tex) else { continue };
                let h = v.iter().fold(mesh_hash, |h, v| hash_vertex(h, v));
                let i = self.tris.len() as u32;
                let (s0, s1) = (tri.y0 as usize / STRIP, (tri.y1 as usize - 1) / STRIP);
                let s1 = s1.min(strips - 1);
                for (bin, sh) in self.bins[s0..=s1].iter_mut().zip(&mut hashes[s0..=s1]) {
                    bin.push(i);
                    *sh = mix(*sh, h);
                }
                self.tris.push(tri);
            }
        }

        let dirty: Vec<bool> = hashes.iter().zip(&self.strip_hashes).map(|(a, b)| resized || a != b).collect();
        self.strip_hashes = hashes;

        let (tris, bins, store) = (&self.tris, &self.bins, &self.store);
        self.fb.par_chunks_mut(STRIP * width).enumerate().filter(|(s, _)| dirty[*s]).for_each(|(s, rows)| {
            rows.fill(clear_u32);
            let y_base = (s * STRIP) as i32;
            let y_end = y_base + (rows.len() / width) as i32;
            for &i in &bins[s] {
                raster(&tris[i as usize], rows, width, y_base, y_end, store);
            }
        });

        let mut damage: Vec<(usize, usize)> = Vec::new();
        for s in (0..strips).filter(|&s| dirty[s]) {
            let (y0, y1) = (s * STRIP, ((s + 1) * STRIP).min(height));
            match damage.last_mut() {
                Some(last) if last.1 == y0 => last.1 = y1,
                _ => damage.push((y0, y1)),
            }
        }
        Painted { damage }
    }
}

#[inline]
fn mix(h: u64, v: u64) -> u64 {
    (h.rotate_left(5) ^ v).wrapping_mul(0x517C_C1B7_2722_0A95)
}

fn hash_vertex(h: u64, v: &Vertex) -> u64 {
    let h = mix(h, v.pos.x.to_bits() as u64 | ((v.pos.y.to_bits() as u64) << 32));
    let h = mix(h, v.uv.x.to_bits() as u64 | ((v.uv.y.to_bits() as u64) << 32));
    mix(h, u32::from_le_bytes(v.color.to_array()) as u64)
}

#[inline]
fn pack(r: u32, g: u32, b: u32) -> u32 {
    (r << 16) | (g << 8) | b
}

fn setup(v: [&Vertex; 3], ppp: f32, clip: [i32; 4], tex: Option<(usize, &Texture)>) -> Option<Tri> {
    let p = v.map(|v| [v.pos.x * ppp, v.pos.y * ppp]);
    let fx = p.map(|p| (p[0] * SUB as f32).round() as i64);
    let fy = p.map(|p| (p[1] * SUB as f32).round() as i64);
    let area = (fx[1] - fx[0]) * (fy[2] - fy[0]) - (fy[1] - fy[0]) * (fx[2] - fx[0]);
    if area == 0 {
        return None;
    }
    let order = if area > 0 { [0, 1, 2] } else { [0, 2, 1] };

    let (min_x, max_x) = p.iter().fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), p| (lo.min(p[0]), hi.max(p[0])));
    let (min_y, max_y) = p.iter().fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), p| (lo.min(p[1]), hi.max(p[1])));
    let x0 = (min_x.floor() as i32).max(clip[0]);
    let x1 = (max_x.ceil() as i32 + 1).min(clip[2]);
    let y0 = (min_y.floor() as i32).max(clip[1]);
    let y1 = (max_y.ceil() as i32 + 1).min(clip[3]);
    if x0 >= x1 || y0 >= y1 {
        return None;
    }

    // Edge a->b: E(P) = dx*(Py - ay) - dy*(Px - ax), positive inside. Pixel
    // centres sit at (16*px + 8, 16*py + 8). The top-left rule draws pixels on
    // an edge shared by two triangles exactly once.
    let half = SUB / 2;
    let mut edges = [[0i64; 3]; 3];
    for (k, e) in edges.iter_mut().enumerate() {
        let (a, b) = (order[k], order[(k + 1) % 3]);
        let (dx, dy) = (fx[b] - fx[a], fy[b] - fy[a]);
        let top_left = dy < 0 || (dy == 0 && dx > 0);
        let bias = if top_left { 0 } else { -1 };
        *e = [-SUB * dy, SUB * dx, dx * (half - fy[a]) - dy * (half - fx[a]) + bias];
    }

    let colors = v.map(|v| v.color.to_array().map(f32::from));
    let same_color = v[0].color == v[1].color && v[1].color == v[2].color;
    let same_uv = v[0].uv == v[1].uv && v[1].uv == v[2].uv;
    let float_area = (p[1][0] - p[0][0]) * (p[2][1] - p[0][1]) - (p[2][0] - p[0][0]) * (p[1][1] - p[0][1]);
    let inv_area = if float_area.abs() > f32::EPSILON { 1.0 / float_area } else { 0.0 };
    let plane = |f: [f32; 3]| Plane::new(&p, f, inv_area);
    let color_planes = |c: [[f32; 4]; 3]| [0, 1, 2, 3].map(|i| plane([c[0][i], c[1][i], c[2][i]]));

    let shade = match tex {
        // A constant UV samples one texel (egui's white pixel for shapes): fold it into the colour.
        Some((_, t)) if same_uv => {
            let texel = t.sample(v[0].uv.x * t.w as f32, v[0].uv.y * t.h as f32);
            let tint = |c: [f32; 4]| [0, 1, 2, 3].map(|i| c[i] * texel[i] / 255.0);
            if same_color { solid(tint(colors[0])) } else { Shade::Gouraud(color_planes(colors.map(tint))) }
        }
        Some((slot, t)) => Shade::Textured {
            tex: slot,
            color: color_planes(colors),
            u: plane(v.map(|v| v.uv.x * t.w as f32)),
            v: plane(v.map(|v| v.uv.y * t.h as f32)),
        },
        None if same_color => solid(colors[0]),
        None => Shade::Gouraud(color_planes(colors)),
    };
    if let Shade::Solid { rgba: [0, 0, 0, 0], .. } = shade {
        return None;
    }
    Some(Tri { edges, x0, x1, y0, y1, shade })
}

fn solid(c: [f32; 4]) -> Shade {
    let rgba = c.map(|x| x.round().clamp(0.0, 255.0) as u32);
    let opaque = (rgba[3] == 255).then(|| pack(rgba[0], rgba[1], rgba[2]));
    Shade::Solid { rgba, opaque }
}

/// `dst = src + dst * (1 - src_alpha)` for premultiplied 8-bit colour.
#[inline]
fn blend_int(dst: u32, src: [u32; 4]) -> u32 {
    let inv = 255 - src[3];
    let ch = |shift: u32, s: u32| (s + (((dst >> shift) & 0xFF) * inv + 127) / 255).min(255);
    pack(ch(16, src[0]), ch(8, src[1]), ch(0, src[2]))
}

#[inline]
fn blend_f(dst: u32, c: [f32; 4]) -> u32 {
    let inv = 1.0 - c[3].clamp(0.0, 255.0) / 255.0;
    let ch = |shift: u32, s: f32| (s + ((dst >> shift) & 0xFF) as f32 * inv + 0.5).clamp(0.0, 255.0) as u32;
    pack(ch(16, c[0]), ch(8, c[1]), ch(0, c[2]))
}

/// The pixel span `[lo, hi]` of row `py` inside all three edges, if any.
#[inline]
fn span(t: &Tri, py: i32) -> Option<(i32, i32)> {
    let (mut lo, mut hi) = (t.x0 as i64, t.x1 as i64 - 1);
    for e in &t.edges {
        let base = e[1] * py as i64 + e[2];
        let a = e[0];
        // Inside where base + a*px >= 0.
        if a > 0 {
            lo = lo.max(-base.div_euclid(a));
        } else if a < 0 {
            hi = hi.min(base.div_euclid(-a));
        } else if base < 0 {
            return None;
        }
    }
    (lo <= hi).then_some((lo as i32, hi as i32))
}

fn raster(t: &Tri, rows: &mut [u32], width: usize, y_base: i32, y_end: i32, store: &[Option<Texture>]) {
    for py in t.y0.max(y_base)..t.y1.min(y_end) {
        let Some((lo, hi)) = span(t, py) else { continue };
        let row = &mut rows[(py - y_base) as usize * width..][..width];
        let px = &mut row[lo as usize..=hi as usize];
        let (xc, yc) = (lo as f32 + 0.5, py as f32 + 0.5);
        match &t.shade {
            Shade::Solid { opaque: Some(c), .. } => px.fill(*c),
            Shade::Solid { rgba, .. } => {
                for d in px {
                    *d = blend_int(*d, *rgba);
                }
            }
            Shade::Gouraud(c) => {
                let mut v = c.map(|p| p.at(xc, yc));
                for d in px {
                    *d = blend_f(*d, v);
                    for i in 0..4 {
                        v[i] += c[i].a;
                    }
                }
            }
            Shade::Textured { tex, color, u, v } => {
                let Some(tex) = &store[*tex] else { continue };
                let mut col = color.map(|p| p.at(xc, yc));
                let (mut tu, mut tv) = (u.at(xc, yc), v.at(xc, yc));
                for d in px {
                    let s = tex.sample(tu, tv);
                    *d = blend_f(*d, [0, 1, 2, 3].map(|i| col[i] * s[i] / 255.0));
                    for i in 0..4 {
                        col[i] += color[i].a;
                    }
                    tu += u.a;
                    tv += v.a;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::epaint::{Mesh, Rect, pos2};

    fn rect(r: Rect, color: Color32) -> ClippedPrimitive {
        let mut mesh = Mesh::default();
        mesh.add_colored_rect(r, color);
        ClippedPrimitive { clip_rect: Rect::EVERYTHING, primitive: Primitive::Mesh(mesh) }
    }

    #[test]
    fn fills_rects_exactly_and_tracks_damage() {
        let mut sr = SoftRenderer::new();
        let prims = vec![rect(Rect::from_min_max(pos2(2.0, 3.0), pos2(6.0, 40.0)), Color32::RED)];
        assert_eq!(sr.paint(10, 48, 1.0, Color32::BLACK, &prims).damage, vec![(0, 48)]);
        let at = |sr: &SoftRenderer, x: usize, y: usize| sr.pixels()[y * 10 + x];
        assert_eq!(at(&sr, 2, 3), 0xFF0000);
        assert_eq!(at(&sr, 5, 39), 0xFF0000);
        for (x, y) in [(1, 3), (6, 3), (2, 2), (2, 40)] {
            assert_eq!(at(&sr, x, y), 0, "({x},{y}) should be outside");
        }
        // An identical frame redraws nothing.
        assert!(sr.paint(10, 48, 1.0, Color32::BLACK, &prims).damage.is_empty());
        // A change in the last strip redraws only that strip.
        let mut more = prims.clone();
        more.push(rect(Rect::from_min_max(pos2(0.0, 44.0), pos2(1.0, 45.0)), Color32::WHITE));
        assert_eq!(sr.paint(10, 48, 1.0, Color32::BLACK, &more).damage, vec![(32, 48)]);
        assert_eq!(at(&sr, 0, 44), 0xFFFFFF);
        assert_eq!(at(&sr, 2, 3), 0xFF0000, "clean strips keep their pixels");
    }

    #[test]
    fn shared_edges_blend_once() {
        let mut sr = SoftRenderer::new();
        let c = Color32::from_rgba_premultiplied(0, 0, 128, 128);
        sr.paint(16, 16, 1.0, Color32::BLACK, &[rect(Rect::from_min_max(pos2(0.0, 0.0), pos2(16.0, 16.0)), c)]);
        assert!(sr.pixels().iter().all(|&p| p == 128), "{:x?}", &sr.pixels()[..16]);
    }

    #[test]
    fn clip_rect_and_scale_apply() {
        let mut sr = SoftRenderer::new();
        let mut prim = rect(Rect::from_min_max(pos2(0.0, 0.0), pos2(8.0, 8.0)), Color32::GREEN);
        prim.clip_rect = Rect::from_min_max(pos2(0.0, 0.0), pos2(4.0, 8.0));
        sr.paint(16, 16, 2.0, Color32::BLACK, &[prim]);
        assert_eq!(sr.pixels()[15 * 16 + 7], 0x00FF00);
        assert_eq!(sr.pixels()[15 * 16 + 8], 0, "clipped at 4 points = 8 pixels");
    }
}
