//! Icon engine: finds, ranks, cleans up and saves recipe icons.
//!
//! Replaces `Get-YardstickIcon.ps1`, whose single source (a filtered Bing image
//! scrape) mostly returned unrelated photos, and whose clean-up damaged icons.
//! Here:
//! * candidates come from several sources, most authoritative first - the
//!   installer's own icon (EXE/MSI resources), Wikidata/Wikimedia Commons,
//!   Chocolatey's curated package icons, the vendor's website (touch icons,
//!   web manifest, favicon), and only then a relevance-filtered image search;
//! * every candidate is scored (source, real resolution, squareness,
//!   transparency, photo-likeness) and identical images found by several
//!   sources are merged, which raises their score;
//! * EXE icons are read from the resource table at their true size instead of
//!   asking Windows to stretch a 32 px icon to 256 px; SVGs are rasterised;
//! * background removal flood-fills from the edges, so white inside the logo
//!   survives; resampling is premultiplied, so there are no dark halos;
//! * output is square, at most 512 px and 750 KB (Intune's limits).

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
pub use image::RgbaImage;
use image::Rgba;
use regex::Regex;
use serde::Serialize;

/// Intune's limits for an app icon.
pub const MAX_SIDE: u32 = 512;
pub const MAX_BYTES: usize = 750 * 1024;

const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36 Edg/141.0.0.0";
const STUDIO_UA: &str = "YardstickDevelopmentStudio/0.1 (https://github.com/jeffpearson1/Yardstick)";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub enum SourceKind {
    Installer,
    File,
    Wikidata,
    Chocolatey,
    Website,
    ImageSearch,
}

impl SourceKind {
    pub fn label(self) -> &'static str {
        match self {
            SourceKind::Installer => "Installer",
            SourceKind::File => "File",
            SourceKind::Wikidata => "Wikidata",
            SourceKind::Chocolatey => "Chocolatey",
            SourceKind::Website => "Vendor site",
            SourceKind::ImageSearch => "Image search",
        }
    }
}

/// What to look for.
#[derive(Debug, Clone, Default)]
pub struct IconQuery {
    pub name: String,
    pub publisher: String,
    /// A vendor page (the recipe's `url`, or a link from the request).
    pub vendor_url: String,
    /// Installers or images to read icons from.
    pub files: Vec<PathBuf>,
    /// Skip network sources.
    pub offline: bool,
}

/// A decoded, scored icon.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub source: SourceKind,
    pub label: String,
    /// Where it came from (URL or file path).
    pub origin: String,
    pub image: RgbaImage,
    /// Pixel size of the original (before any downscale for preview).
    pub native: (u32, u32),
    pub vector: bool,
    pub prior: f32,
    pub score: f32,
    pub reasons: Vec<String>,
    /// Other sources that returned the same picture.
    pub also_from: Vec<String>,
    hash: u64,
}

#[derive(Debug)]
pub enum SearchEvent {
    Status(String),
    Candidate(Box<Candidate>),
    Finished,
}

// ---- decoding -----------------------------------------------------------

/// Largest preview kept in memory; the native size is remembered separately.
const KEEP_SIDE: u32 = 1024;

pub struct Decoded {
    pub image: RgbaImage,
    pub native: (u32, u32),
    pub vector: bool,
}

pub fn decode(bytes: &[u8]) -> Result<Decoded> {
    if looks_like_svg(bytes) {
        return render_svg(bytes, MAX_SIDE);
    }
    let format = image::guess_format(bytes).context("not a recognised image format")?;
    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(512 * 1024 * 1024);
    reader.limits(limits);
    let img = reader.decode().context("could not decode image")?.to_rgba8();
    let native = img.dimensions();
    let image = if native.0.max(native.1) > KEEP_SIDE { resize_fit(&img, KEEP_SIDE) } else { img };
    Ok(Decoded { image, native, vector: false })
}

fn looks_like_svg(bytes: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(1024)]).to_ascii_lowercase();
    let head = head.trim_start_matches('\u{feff}').trim_start();
    head.starts_with("<svg") || (head.starts_with("<?xml") || head.starts_with("<!doctype svg") || head.starts_with("<!--")) && head.contains("<svg")
}

fn fontdb() -> Arc<resvg::usvg::fontdb::Database> {
    static DB: OnceLock<Arc<resvg::usvg::fontdb::Database>> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = resvg::usvg::fontdb::Database::new();
        db.load_system_fonts();
        Arc::new(db)
    })
    .clone()
}

pub fn render_svg(bytes: &[u8], side: u32) -> Result<Decoded> {
    let opt = resvg::usvg::Options { fontdb: fontdb(), ..Default::default() };
    let tree = resvg::usvg::Tree::from_data(bytes, &opt).context("invalid SVG")?;
    let size = tree.size();
    let scale = side as f32 / size.width().max(size.height()).max(1.0);
    let (w, h) = (((size.width() * scale).round() as u32).max(1), ((size.height() * scale).round() as u32).max(1));
    let mut pixmap = resvg::tiny_skia::Pixmap::new(w, h).ok_or_else(|| anyhow!("SVG has no size"))?;
    resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    let mut img = RgbaImage::new(w, h);
    for (dst, src) in img.pixels_mut().zip(pixmap.pixels()) {
        let c = src.demultiply();
        *dst = Rgba([c.red(), c.green(), c.blue(), c.alpha()]);
    }
    Ok(Decoded { image: img, native: (w, h), vector: true })
}

/// Icons embedded in an installer, executable, MSI, or an image file.
pub fn from_file(path: &Path) -> Result<Vec<Candidate>> {
    let mut head = [0u8; 8];
    let n = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?.read(&mut head)?;
    let head = &head[..n];
    let name = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let origin = path.display().to_string();
    let mk = |d: Decoded, label: String, prior: f32| finish(SourceKind::Installer, label, origin.clone(), d, prior);
    if head.starts_with(b"MZ") {
        let icons = pe_icons(path)?;
        if icons.is_empty() {
            bail!("{name} has no icon resources.");
        }
        return Ok(icons.into_iter().enumerate().map(|(i, d)| mk(d, format!("{name} icon #{}", i + 1), if i == 0 { 0.92 } else { 0.8 })).collect());
    }
    if head.starts_with(&[0xD0, 0xCF, 0x11, 0xE0]) {
        let icons = msi_icons(path)?;
        if icons.is_empty() {
            bail!("{name} has no Icon table entries.");
        }
        return Ok(icons.into_iter().map(|(label, d, product)| mk(d, format!("{name}: {label}"), if product { 0.92 } else { 0.8 })).collect());
    }
    let bytes = std::fs::read(path)?;
    let d = decode(&bytes)?;
    Ok(vec![finish(SourceKind::File, name, origin, d, 0.9)])
}

/// Every icon group in a PE file, largest frame of each, main icon first.
fn pe_icons(path: &Path) -> Result<Vec<Decoded>> {
    use pelite::pe64::Pe as _;
    let map = pelite::FileMap::open(path).with_context(|| format!("mapping {}", path.display()))?;
    let resources = match pelite::PeFile::from_bytes(map.as_ref()).context("not a valid PE file")? {
        pelite::Wrap::T32(f) => pelite::pe32::Pe::resources(f),
        pelite::Wrap::T64(f) => f.resources(),
    };
    let Ok(resources) = resources else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for entry in resources.icons().take(8) {
        let Ok((_name, group)) = entry else { continue };
        let mut ico = Vec::new();
        if group.write(&mut ico).is_err() {
            continue;
        }
        if let Ok(d) = decode(&ico) {
            out.push(d);
        }
    }
    Ok(out)
}

/// The MSI `Icon` table (each entry is an ICO or EXE), ARPPRODUCTICON first.
fn msi_icons(path: &Path) -> Result<Vec<(String, Decoded, bool)>> {
    let mut package = msi::open(path).with_context(|| format!("opening {} as MSI", path.display()))?;
    if !package.has_table("Icon") {
        return Ok(Vec::new());
    }
    let product_icon = package
        .select_rows(msi::Select::table("Property"))
        .ok()
        .and_then(|rows| {
            rows.into_iter().find(|r| r["Property"].as_str() == Some("ARPPRODUCTICON")).and_then(|r| r["Value"].as_str().map(str::to_string))
        })
        .unwrap_or_default();
    let names: Vec<String> = package.select_rows(msi::Select::table("Icon"))?.filter_map(|r| r["Name"].as_str().map(str::to_string)).collect();
    let mut out = Vec::new();
    for name in names {
        let mut bytes = Vec::new();
        let Ok(mut stream) = package.read_stream(&format!("Icon.{name}")) else { continue };
        if stream.read_to_end(&mut bytes).is_err() {
            continue;
        }
        let decoded = if bytes.starts_with(b"MZ") {
            let tmp = std::env::temp_dir().join(format!("ystudio-msi-icon-{}.exe", std::process::id()));
            std::fs::write(&tmp, &bytes)?;
            let r = pe_icons(&tmp);
            let _ = std::fs::remove_file(&tmp);
            r.ok().and_then(|v| v.into_iter().next())
        } else {
            decode(&bytes).ok()
        };
        if let Some(d) = decoded {
            out.push((name.clone(), d, name == product_icon));
        }
    }
    out.sort_by_key(|(_, _, product)| !*product);
    Ok(out)
}

// ---- scoring -------------------------------------------------------------

fn finish(source: SourceKind, label: String, origin: String, d: Decoded, prior: f32) -> Candidate {
    let mut c = Candidate { source, label, origin, image: d.image, native: d.native, vector: d.vector, prior, score: 0.0, reasons: Vec::new(), also_from: Vec::new(), hash: 0 };
    c.hash = ahash(&c.image);
    rescore(&mut c);
    c
}

/// Recomputes [`Candidate::score`] and its explanation.
pub fn rescore(c: &mut Candidate) {
    let mut s = c.prior;
    let mut why = vec![format!("{} ({:.0})", c.source.label(), c.prior * 100.0)];
    let side = c.native.0.min(c.native.1);
    if c.vector {
        s += 0.15;
        why.push("vector (+15)".into());
    } else {
        let (d, label) = match side {
            256.. => (0.15, "≥256 px"),
            128..=255 => (0.08, "≥128 px"),
            64..=127 => (0.0, "64-127 px"),
            32..=63 => (-0.15, "small (32-63 px)"),
            _ => (-0.3, "tiny (<32 px)"),
        };
        s += d;
        why.push(format!("{label} ({:+.0})", d * 100.0));
    }
    let stats = analyse(&c.image);
    let aspect = stats.content_aspect;
    if aspect <= 1.15 {
        s += 0.1;
        why.push("square (+10)".into());
    } else if aspect > 1.8 {
        s -= 0.25;
        why.push(format!("wide {aspect:.1}:1 - wordmark or photo (-25)"));
    } else if aspect > 1.3 {
        s -= 0.08;
        why.push(format!("{aspect:.1}:1 (-8)"));
    }
    if stats.transparent_border {
        s += 0.08;
        why.push("transparent background (+8)".into());
    }
    if stats.photo_like {
        s -= 0.2;
        why.push("looks like a photo/screenshot (-20)".into());
    }
    if !c.also_from.is_empty() {
        let d = 0.06 * c.also_from.len().min(3) as f32;
        s += d;
        why.push(format!("also found via {} (+{:.0})", c.also_from.join(", "), d * 100.0));
    }
    c.score = s.clamp(0.0, 1.0);
    c.reasons = why;
}

struct Stats {
    content_aspect: f32,
    transparent_border: bool,
    photo_like: bool,
}

fn analyse(img: &RgbaImage) -> Stats {
    let bounds = content_bounds(img, 16).unwrap_or((0, 0, img.width(), img.height()));
    let (w, h) = (bounds.2.max(1) as f32, bounds.3.max(1) as f32);
    let border = border_pixels(img);
    let transparent_border = !border.is_empty() && border.iter().filter(|p| p[3] < 32).count() * 10 >= border.len() * 8;
    // Photos have smooth gradients everywhere: many distinct colours and no alpha.
    let small = if img.width().max(img.height()) > 64 { resize_fit(img, 64) } else { img.clone() };
    let mut colours = HashSet::new();
    let mut opaque = 0usize;
    for p in small.pixels() {
        if p[3] > 250 {
            opaque += 1;
        }
        colours.insert((p[0] >> 3, p[1] >> 3, p[2] >> 3));
    }
    let n = (small.width() * small.height()) as usize;
    let photo_like = opaque * 100 >= n * 98 && colours.len() * 3 > n && !uniform_border(img, 24);
    Stats { content_aspect: w.max(h) / w.min(h), transparent_border, photo_like }
}

/// 8x8 average hash of the image over white, for de-duplication.
fn ahash(img: &RgbaImage) -> u64 {
    let mut flat = img.clone();
    for p in flat.pixels_mut() {
        let a = p[3] as u32;
        for c in 0..3 {
            p[c] = ((p[c] as u32 * a + 255 * (255 - a)) / 255) as u8;
        }
        p[3] = 255;
    }
    let trimmed = match content_bounds(img, 16) {
        Some((x, y, w, h)) => image::imageops::crop_imm(&flat, x, y, w, h).to_image(),
        None => flat,
    };
    let small = image::imageops::resize(&trimmed, 8, 8, image::imageops::FilterType::Triangle);
    let lum: Vec<u32> = small.pixels().map(|p| (p[0] as u32 * 3 + p[1] as u32 * 6 + p[2] as u32) / 10).collect();
    let avg = lum.iter().sum::<u32>() / 64;
    lum.iter().enumerate().fold(0u64, |h, (i, l)| if *l > avg { h | (1 << i) } else { h })
}

/// Adds `c` to `list`, merging it into an existing near-identical candidate
/// (keeping the better copy) and re-sorting by score.
pub fn merge(list: &mut Vec<Candidate>, mut c: Candidate) {
    let aspect = |c: &Candidate| c.native.0 as f32 / c.native.1.max(1) as f32;
    if let Some(i) = list.iter().position(|o| o.origin == c.origin || ((o.hash ^ c.hash).count_ones() <= 4 && (aspect(o) / aspect(&c) - 1.0).abs() < 0.15)) {
        let o = &mut list[i];
        if o.origin == c.origin {
            return;
        }
        let better_is_new = (c.vector && !o.vector) || (c.vector == o.vector && c.native.0 * c.native.1 > o.native.0 * o.native.1);
        let (keep, other) = if better_is_new { (&mut c, o.clone()) } else { (&mut *o, c.clone()) };
        let tag = other.source.label().to_string();
        if other.source != keep.source && !keep.also_from.contains(&tag) {
            keep.also_from.push(tag);
        }
        for t in other.also_from {
            if t != keep.source.label() && !keep.also_from.contains(&t) {
                keep.also_from.push(t);
            }
        }
        keep.prior = keep.prior.max(other.prior);
        rescore(keep);
        if better_is_new {
            list[i] = c;
        }
    } else {
        list.push(c);
    }
    list.sort_by(|a, b| b.score.total_cmp(&a.score));
}

// ---- processing ----------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Process {
    /// Make the edge-connected background transparent.
    pub remove_background: bool,
    /// Colour distance (0-255, per channel) treated as background.
    pub tolerance: u8,
    /// Crop transparent margins.
    pub trim: bool,
    /// Pad to a square canvas.
    pub square: bool,
    /// Padding around the content, as a fraction of the side (0-0.3).
    pub margin: f32,
    /// Largest side of the output; smaller images are not upscaled.
    pub max_side: u32,
    /// Fill the transparent area with this colour.
    pub fill: Option<[u8; 3]>,
}

impl Default for Process {
    fn default() -> Self {
        Process { remove_background: false, tolerance: 24, trim: true, square: true, margin: 0.0, max_side: MAX_SIDE, fill: None }
    }
}

impl Process {
    /// Sensible settings for a candidate: remove a flat, opaque background.
    pub fn auto(img: &RgbaImage) -> Self {
        let border = border_pixels(img);
        let opaque = border.iter().filter(|p| p[3] > 250).count() * 10 >= border.len() * 9;
        Process { remove_background: opaque && uniform_border(img, 24), ..Default::default() }
    }
}

pub fn process(src: &RgbaImage, p: &Process) -> RgbaImage {
    let mut img = src.clone();
    if p.remove_background {
        remove_background(&mut img, p.tolerance);
    }
    if p.trim
        && let Some((x, y, w, h)) = content_bounds(&img, 8)
    {
        img = image::imageops::crop_imm(&img, x, y, w, h).to_image();
    }
    if p.square || p.margin > 0.0 {
        let side = img.width().max(img.height());
        let (cw, ch) = if p.square { (side, side) } else { (img.width(), img.height()) };
        let pad = (side as f32 * p.margin.clamp(0.0, 0.3) / (1.0 - 2.0 * p.margin.clamp(0.0, 0.3))).round() as u32;
        let mut canvas = RgbaImage::new(cw + 2 * pad, ch + 2 * pad);
        let (ox, oy) = (pad + (cw - img.width()) / 2, pad + (ch - img.height()) / 2);
        image::imageops::replace(&mut canvas, &img, ox as i64, oy as i64);
        img = canvas;
    }
    if img.width().max(img.height()) > p.max_side {
        img = resize_fit(&img, p.max_side);
    }
    if let Some([r, g, b]) = p.fill {
        for px in img.pixels_mut() {
            let a = px[3] as u32;
            *px = Rgba([
                ((px[0] as u32 * a + r as u32 * (255 - a)) / 255) as u8,
                ((px[1] as u32 * a + g as u32 * (255 - a)) / 255) as u8,
                ((px[2] as u32 * a + b as u32 * (255 - a)) / 255) as u8,
                255,
            ]);
        }
    }
    img
}

/// Resizes to fit `side` with premultiplied Lanczos3 (no dark fringes).
pub fn resize_fit(img: &RgbaImage, side: u32) -> RgbaImage {
    let (w, h) = img.dimensions();
    let scale = side as f32 / w.max(h).max(1) as f32;
    let (nw, nh) = (((w as f32 * scale).round() as u32).max(1), ((h as f32 * scale).round() as u32).max(1));
    let mut f = image::Rgba32FImage::new(w, h);
    for (d, s) in f.pixels_mut().zip(img.pixels()) {
        let a = s[3] as f32 / 255.0;
        *d = Rgba([s[0] as f32 / 255.0 * a, s[1] as f32 / 255.0 * a, s[2] as f32 / 255.0 * a, a]);
    }
    let filter = if scale < 1.0 { image::imageops::FilterType::Lanczos3 } else { image::imageops::FilterType::CatmullRom };
    let r = image::imageops::resize(&f, nw, nh, filter);
    let mut out = RgbaImage::new(nw, nh);
    for (d, s) in out.pixels_mut().zip(r.pixels()) {
        let a = s[3].clamp(0.0, 1.0);
        let un = |c: f32| if a > 1e-4 { (c / a).clamp(0.0, 1.0) } else { 0.0 };
        *d = Rgba([(un(s[0]) * 255.0).round() as u8, (un(s[1]) * 255.0).round() as u8, (un(s[2]) * 255.0).round() as u8, (a * 255.0).round() as u8]);
    }
    out
}

fn border_pixels(img: &RgbaImage) -> Vec<Rgba<u8>> {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return Vec::new();
    }
    let mut v = Vec::with_capacity(2 * (w + h) as usize);
    for x in 0..w {
        v.push(*img.get_pixel(x, 0));
        v.push(*img.get_pixel(x, h - 1));
    }
    for y in 1..h.saturating_sub(1) {
        v.push(*img.get_pixel(0, y));
        v.push(*img.get_pixel(w - 1, y));
    }
    v
}

fn dist(p: &Rgba<u8>, c: [u8; 3]) -> u8 {
    (0..3).map(|i| p[i].abs_diff(c[i])).max().unwrap_or(0)
}

/// Most common opaque border colour and the share of the border close to it.
fn border_colour(img: &RgbaImage, tol: u8) -> Option<([u8; 3], f32)> {
    let border = border_pixels(img);
    let opaque: Vec<&Rgba<u8>> = border.iter().filter(|p| p[3] > 200).collect();
    if opaque.is_empty() {
        return None;
    }
    let mut counts = std::collections::HashMap::new();
    for p in &opaque {
        *counts.entry((p[0] >> 4, p[1] >> 4, p[2] >> 4)).or_insert(0usize) += 1;
    }
    let ((r, g, b), _) = counts.into_iter().max_by_key(|(_, n)| *n)?;
    let seed_px: Vec<&&Rgba<u8>> = opaque.iter().filter(|p| (p[0] >> 4, p[1] >> 4, p[2] >> 4) == (r, g, b)).collect();
    let avg = |i: usize| (seed_px.iter().map(|p| p[i] as u32).sum::<u32>() / seed_px.len() as u32) as u8;
    let seed = [avg(0), avg(1), avg(2)];
    let close = border.iter().filter(|p| p[3] > 200 && dist(p, seed) <= tol).count();
    Some((seed, close as f32 / border.len() as f32))
}

fn uniform_border(img: &RgbaImage, tol: u8) -> bool {
    border_colour(img, tol).is_some_and(|(_, share)| share >= 0.85)
}

/// Flood-fills the background inwards from the edges. Only pixels connected to
/// the border are removed, so the same colour inside the artwork survives.
/// Anti-aliased edge pixels get partial alpha with the background unmixed.
pub fn remove_background(img: &mut RgbaImage, tol: u8) {
    let Some((seed, _)) = border_colour(img, tol) else { return };
    let (w, h) = img.dimensions();
    let idx = |x: u32, y: u32| (y * w + x) as usize;
    let mut bg = vec![false; (w * h) as usize];
    let mut stack: Vec<(u32, u32)> = Vec::new();
    for x in 0..w {
        stack.push((x, 0));
        stack.push((x, h - 1));
    }
    for y in 0..h {
        stack.push((0, y));
        stack.push((w - 1, y));
    }
    while let Some((x, y)) = stack.pop() {
        let i = idx(x, y);
        if bg[i] {
            continue;
        }
        let p = img.get_pixel(x, y);
        if p[3] > 0 && dist(p, seed) > tol {
            continue;
        }
        bg[i] = true;
        if x > 0 {
            stack.push((x - 1, y));
        }
        if y > 0 {
            stack.push((x, y - 1));
        }
        if x + 1 < w {
            stack.push((x + 1, y));
        }
        if y + 1 < h {
            stack.push((x, y + 1));
        }
    }
    let soft = tol.max(1) as f32;
    let mut out = img.clone();
    for y in 0..h {
        for x in 0..w {
            let i = idx(x, y);
            if bg[i] {
                out.put_pixel(x, y, Rgba([0, 0, 0, 0]));
                continue;
            }
            let touches = [(-1i32, 0i32), (1, 0), (0, -1), (0, 1)].iter().any(|(dx, dy)| {
                let (nx, ny) = (x as i32 + dx, y as i32 + dy);
                nx >= 0 && ny >= 0 && (nx as u32) < w && (ny as u32) < h && bg[idx(nx as u32, ny as u32)]
            });
            let p = *img.get_pixel(x, y);
            let d = dist(&p, seed) as f32;
            if touches && d < 2.0 * soft {
                let f = ((d - soft) / soft).clamp(0.0, 1.0);
                let a = (p[3] as f32 * f).round() as u8;
                let un = |c: u8, b: u8| if f > 0.0 { ((c as f32 - (1.0 - f) * b as f32) / f).clamp(0.0, 255.0).round() as u8 } else { 0 };
                out.put_pixel(x, y, if a == 0 { Rgba([0, 0, 0, 0]) } else { Rgba([un(p[0], seed[0]), un(p[1], seed[1]), un(p[2], seed[2]), a]) });
            }
        }
    }
    *img = out;
}

/// Bounding box of pixels with alpha above `min_alpha`, as (x, y, w, h).
pub fn content_bounds(img: &RgbaImage, min_alpha: u8) -> Option<(u32, u32, u32, u32)> {
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
    for (x, y, p) in img.enumerate_pixels() {
        if p[3] > min_alpha {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
    }
    (x0 != u32::MAX).then(|| (x0, y0, x1 - x0 + 1, y1 - y0 + 1))
}

/// PNG bytes within Intune's limits (shrinking if the encoding is too big).
pub fn encode_png(img: &RgbaImage) -> Result<Vec<u8>> {
    let mut img = if img.width().max(img.height()) > MAX_SIDE { resize_fit(img, MAX_SIDE) } else { img.clone() };
    loop {
        let mut bytes = Vec::new();
        let enc = image::codecs::png::PngEncoder::new_with_quality(&mut bytes, image::codecs::png::CompressionType::Best, image::codecs::png::FilterType::Adaptive);
        image::ImageEncoder::write_image(enc, img.as_raw(), img.width(), img.height(), image::ExtendedColorType::Rgba8)?;
        if bytes.len() <= MAX_BYTES || img.width() <= 64 {
            return Ok(bytes);
        }
        let side = (img.width().max(img.height()) as f32 * 0.8) as u32;
        img = resize_fit(&img, side);
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Saved {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub bytes: usize,
}

pub fn save_png(img: &RgbaImage, path: &Path) -> Result<Saved> {
    let bytes = encode_png(img)?;
    let decoded = image::load_from_memory(&bytes)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, &bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(Saved { path: path.to_path_buf(), width: decoded.width(), height: decoded.height(), bytes: bytes.len() })
}

/// `Notepad++` -> `notepadplusplus.png`.
pub fn icon_file_name(name_or_id: &str) -> String {
    let stem = name_or_id.trim().to_lowercase();
    let stem = stem.strip_suffix(".png").unwrap_or(&stem);
    let s = crate::recipe::suggest_id(stem);
    format!("{}.png", if s.is_empty() { "icon" } else { &s })
}

// ---- network -------------------------------------------------------------

fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(15)))
            .tls_config(ureq::tls::TlsConfig::builder().provider(ureq::tls::TlsProvider::NativeTls).root_certs(ureq::tls::RootCerts::PlatformVerifier).build())
            .user_agent(BROWSER_UA)
            .build()
            .into()
    })
}

fn get(url: &str, ua: &str, limit: u64) -> Result<(Vec<u8>, String)> {
    get_accept(url, ua, "text/html,application/xhtml+xml,application/json,image/avif,image/webp,image/*,*/*;q=0.8", limit)
}

fn get_accept(url: &str, ua: &str, accept: &str, limit: u64) -> Result<(Vec<u8>, String)> {
    let mut resp = agent()
        .get(url)
        .header("User-Agent", ua)
        .header("Accept-Language", "en-US,en;q=0.9")
        .header("Accept", accept)
        .call()
        .map_err(|e| anyhow!("{url}: {e}"))?;
    let ctype = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let bytes = resp.body_mut().with_config().limit(limit).read_to_vec().map_err(|e| anyhow!("{url}: {e}"))?;
    Ok((bytes, ctype))
}

fn get_text(url: &str, ua: &str) -> Result<String> {
    Ok(String::from_utf8_lossy(&get(url, ua, 4 * 1024 * 1024)?.0).into_owned())
}

pub fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out += &format!("%{b:02X}"),
        }
    }
    out
}

/// Resolves `href` against the page it appeared on.
pub fn join_url(base: &str, href: &str) -> Option<String> {
    let href = html_unescape(href.trim());
    if href.is_empty() || href.starts_with("data:") || href.starts_with("javascript:") {
        return None;
    }
    if href.starts_with("http://") || href.starts_with("https://") {
        return Some(href);
    }
    let (scheme, rest) = base.split_once("://")?;
    if let Some(r) = href.strip_prefix("//") {
        return Some(format!("{scheme}://{r}"));
    }
    let host = rest.split(['/', '?', '#']).next()?;
    if href.starts_with('/') {
        return Some(format!("{scheme}://{host}{href}"));
    }
    let path = rest[host.len()..].split(['?', '#']).next().unwrap_or("");
    let dir = path.rfind('/').map_or("/", |i| &path[..=i]);
    Some(format!("{scheme}://{host}{dir}{href}"))
}

fn html_unescape(s: &str) -> String {
    s.replace("&amp;", "&").replace("&quot;", "\"").replace("&#39;", "'").replace("&#x2F;", "/").replace("&lt;", "<").replace("&gt;", ">")
}

fn significant_tokens(name: &str) -> Vec<String> {
    const SKIP: &[&str] = &["the", "for", "and", "app", "software", "desktop", "client", "pro", "studio", "inc", "llc"];
    let all: Vec<String> = name
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| t.len() >= 2)
        .map(str::to_string)
        .collect();
    let strong: Vec<String> = all.iter().filter(|t| t.len() >= 3 && !SKIP.contains(&t.as_str())).cloned().collect();
    if strong.is_empty() { all } else { strong }
}

fn relevant(name: &str, haystacks: &[&str]) -> bool {
    let toks = significant_tokens(name);
    let hay = haystacks.join(" ").to_lowercase();
    let squashed: String = hay.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let joined: String = toks.concat();
    toks.iter().any(|t| hay.contains(t.as_str())) || (!joined.is_empty() && squashed.contains(&joined))
}

/// A URL worth downloading and scoring.
#[derive(Debug, Clone)]
pub struct Lead {
    pub source: SourceKind,
    pub label: String,
    pub url: String,
    pub prior: f32,
    pub ua: &'static str,
}

/// Wikidata items for `name`: (label, description, icon leads, official websites).
pub fn wikidata(name: &str) -> Result<(Vec<Lead>, Vec<String>)> {
    let search: serde_json::Value = serde_json::from_str(&get_text(
        &format!("https://www.wikidata.org/w/api.php?action=wbsearchentities&search={}&language=en&format=json&type=item&limit=5", url_encode(name)),
        STUDIO_UA,
    )?)?;
    let hits: Vec<(String, String, String)> = search["search"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|h| Some((h["id"].as_str()?.to_string(), h["label"].as_str().unwrap_or("").to_string(), h["description"].as_str().unwrap_or("").to_string())))
        .filter(|(_, label, desc)| !desc.contains("file format") && !desc.contains("Wikimedia") && relevant(name, &[label]))
        .take(3)
        .collect();
    if hits.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let ids: Vec<&str> = hits.iter().map(|h| h.0.as_str()).collect();
    let ents: serde_json::Value = serde_json::from_str(&get_text(
        &format!("https://www.wikidata.org/w/api.php?action=wbgetentities&ids={}&props=claims&format=json", ids.join("|")),
        STUDIO_UA,
    )?)?;
    let mut leads = Vec::new();
    let mut sites = Vec::new();
    let software_re = Regex::new(r"(?i)software|application|program|browser|editor|client|tool|suite|player|ide\b|system|utility|game|runtime|framework|driver").unwrap();
    for (rank, (id, label, desc)) in hits.iter().enumerate() {
        let claims = &ents["entities"][id]["claims"];
        let values = |p: &str| -> Vec<String> {
            claims[p].as_array().into_iter().flatten().filter_map(|c| c["mainsnak"]["datavalue"]["value"].as_str().map(str::to_string)).collect()
        };
        let software = software_re.is_match(desc);
        let base = if software { 0.0 } else { -0.12 } - rank as f32 * 0.05;
        for (prop, what, prior) in [("P2910", "icon", 0.9), ("P8972", "small logo", 0.84), ("P154", "logo", 0.72)] {
            for file in values(prop).into_iter().take(2) {
                leads.push(Lead {
                    source: SourceKind::Wikidata,
                    label: format!("{label} {what} ({file})"),
                    url: format!("https://commons.wikimedia.org/wiki/Special:FilePath/{}?width=512", url_encode(&file.replace(' ', "_"))),
                    prior: prior + base,
                    ua: STUDIO_UA,
                });
            }
        }
        if rank == 0 || software {
            sites.extend(values("P856").into_iter().take(1));
        }
    }
    Ok((leads, sites))
}

/// Chocolatey community packages whose title matches: (icon leads, project URLs).
pub fn chocolatey(name: &str) -> Result<(Vec<Lead>, Vec<String>)> {
    let xml = String::from_utf8_lossy(
        &get_accept(
            &format!(
                "https://community.chocolatey.org/api/v2/Search()?searchTerm='{}'&targetFramework=''&includePrerelease=false&$top=8",
                url_encode(&name.replace('\'', ""))
            ),
            STUDIO_UA,
            "application/atom+xml,application/xml",
            4 * 1024 * 1024,
        )?
        .0,
    )
    .into_owned();
    Ok(parse_chocolatey(name, &xml))
}

pub fn parse_chocolatey(name: &str, xml: &str) -> (Vec<Lead>, Vec<String>) {
    let field = |entry: &str, f: &str| -> String {
        Regex::new(&format!(r"<d:{f}>([^<]*)</d:{f}>")).unwrap().captures(entry).map(|c| html_unescape(&c[1])).unwrap_or_default()
    };
    let mut leads: Vec<Lead> = Vec::new();
    let mut sites = Vec::new();
    let id_re = Regex::new(r"<d:Id>([^<]*)</d:Id>|Id='([^']*)'").unwrap();
    for entry in xml.split("<entry").skip(1) {
        let title = field(entry, "Title");
        let id = id_re.captures(entry).and_then(|c| c.get(1).or(c.get(2)).map(|m| m.as_str().to_string())).unwrap_or_default();
        if !relevant(name, &[&title, &id]) {
            continue;
        }
        let icon = rawgit_to_jsdelivr(&field(entry, "IconUrl"));
        if !icon.is_empty() && !leads.iter().any(|l| l.url == icon) {
            leads.push(Lead { source: SourceKind::Chocolatey, label: format!("{title} ({id})"), url: icon, prior: 0.8 - leads.len() as f32 * 0.04, ua: BROWSER_UA });
        }
        let site = field(entry, "ProjectUrl");
        if !site.is_empty() && !sites.contains(&site) {
            sites.push(site);
        }
    }
    leads.truncate(3);
    (leads, sites)
}

/// cdn.rawgit.com shut down; jsDelivr serves the same files.
fn rawgit_to_jsdelivr(url: &str) -> String {
    for prefix in ["https://cdn.rawgit.com/", "https://rawgit.com/", "http://cdn.rawgit.com/"] {
        if let Some(rest) = url.strip_prefix(prefix) {
            let parts: Vec<&str> = rest.splitn(4, '/').collect();
            if parts.len() == 4 {
                return format!("https://cdn.jsdelivr.net/gh/{}/{}@{}/{}", parts[0], parts[1], parts[2], parts[3]);
            }
        }
    }
    if let Some(rest) = url.strip_prefix("https://raw.githubusercontent.com/") {
        let parts: Vec<&str> = rest.splitn(4, '/').collect();
        if parts.len() == 4 {
            return format!("https://cdn.jsdelivr.net/gh/{}/{}@{}/{}", parts[0], parts[1], parts[2], parts[3]);
        }
    }
    url.to_string()
}

/// Hosts that serve files for everyone; their favicon is not the product's.
fn is_hosting_site(host: &str) -> bool {
    const HOSTS: &[&str] = &[
        "dropbox.com", "box.com", "sharepoint.com", "onedrive.live.com", "1drv.ms", "drive.google.com", "sourceforge.net", "fosshub.com",
        "softpedia.com", "filehippo.com", "uptodown.com", "download.cnet.com", "microsoft.com", "aka.ms", "amazonaws.com", "cloudfront.net",
        "azureedge.net", "githubusercontent.com", "gitlab.com", "bitbucket.org", "chocolatey.org", "winget.run",
    ];
    HOSTS.iter().any(|h| host == *h || host.ends_with(&format!(".{h}")))
}

/// Icons a vendor page declares: touch icons, sized `<link rel=icon>`s, the web
/// manifest, then the favicon and og:image as weak fallbacks.
pub fn website(url: &str) -> Result<Vec<Lead>> {
    let host = crate::intake::host_of(url).ok_or_else(|| anyhow!("not a URL: {url}"))?;
    if host == "github.com" {
        // github.com/<owner>/<repo>: the owner's avatar is usually the project logo.
        let owner = url.split("github.com/").nth(1).and_then(|r| r.split('/').next()).filter(|o| !o.is_empty());
        return Ok(owner
            .map(|o| vec![Lead { source: SourceKind::Website, label: format!("GitHub avatar of {o}"), url: format!("https://github.com/{o}.png?size=460"), prior: 0.55, ua: BROWSER_UA }])
            .unwrap_or_default());
    }
    if is_hosting_site(&host) {
        return Ok(Vec::new());
    }
    let scheme = if url.starts_with("http://") { "http" } else { "https" };
    let root = format!("{scheme}://{}/", url.split("://").nth(1).and_then(|r| r.split('/').next()).unwrap_or(&host));
    let mut leads = Vec::new();
    let mut pages = vec![url.to_string()];
    if url.trim_end_matches('/') != root.trim_end_matches('/') {
        pages.push(root.clone());
    }
    for page in &pages {
        let Ok(html) = get_text(page, BROWSER_UA) else { continue };
        leads.extend(parse_html_icons(page, &html));
        if let Some(manifest) = find_manifest(page, &html)
            && let Ok(text) = get_text(&manifest, BROWSER_UA)
        {
            leads.extend(parse_manifest(&manifest, &text));
        }
    }
    for (path, label, prior) in [("apple-touch-icon.png", "apple-touch-icon.png", 0.68), ("favicon.ico", "favicon.ico", 0.45)] {
        let u = format!("{root}{path}");
        if !leads.iter().any(|l: &Lead| l.url == u) {
            leads.push(Lead { source: SourceKind::Website, label: format!("{host} {label}"), url: u, prior, ua: BROWSER_UA });
        }
    }
    let mut seen = HashSet::new();
    leads.retain(|l| seen.insert(l.url.clone()));
    for l in &mut leads {
        if !l.label.contains(&host) {
            l.label = format!("{host} {}", l.label);
        }
    }
    Ok(leads)
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let re = Regex::new(&format!(r#"(?i)\s{name}\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#)).ok()?;
    re.captures(tag).and_then(|c| c.get(1).or(c.get(2)).or(c.get(3))).map(|m| m.as_str().to_string())
}

pub fn parse_html_icons(page: &str, html: &str) -> Vec<Lead> {
    let mut out = Vec::new();
    for m in Regex::new(r"(?is)<link\b[^>]*>").unwrap().find_iter(html) {
        let tag = m.as_str();
        let rel = attr(tag, "rel").unwrap_or_default().to_lowercase();
        let Some(href) = attr(tag, "href").and_then(|h| join_url(page, &h)) else { continue };
        let size = attr(tag, "sizes").and_then(|s| s.split(['x', 'X']).next().and_then(|n| n.trim().parse::<u32>().ok())).unwrap_or(0);
        let svg = href.to_lowercase().split('?').next().unwrap_or("").ends_with(".svg") || attr(tag, "type").is_some_and(|t| t.contains("svg"));
        let (label, prior) = if rel.contains("apple-touch-icon") {
            ("apple-touch-icon", 0.72)
        } else if rel.contains("mask-icon") {
            // Monochrome by design.
            ("mask-icon", 0.35)
        } else if rel.split_whitespace().any(|r| r == "icon") {
            if svg {
                ("svg icon", 0.74)
            } else if size >= 128 {
                ("icon", 0.7)
            } else {
                ("icon", 0.5)
            }
        } else if rel.contains("fluid-icon") {
            ("fluid-icon", 0.6)
        } else {
            continue;
        };
        let label = if size > 0 { format!("{label} {size}px") } else { label.to_string() };
        out.push(Lead { source: SourceKind::Website, label, url: href, prior, ua: BROWSER_UA });
    }
    for m in Regex::new(r"(?is)<meta\b[^>]*>").unwrap().find_iter(html) {
        let tag = m.as_str();
        let key = attr(tag, "property").or_else(|| attr(tag, "name")).unwrap_or_default().to_lowercase();
        if (key == "og:image" || key == "msapplication-tileimage")
            && let Some(u) = attr(tag, "content").and_then(|c| join_url(page, &c))
        {
            let prior = if key == "og:image" { 0.3 } else { 0.55 };
            out.push(Lead { source: SourceKind::Website, label: key, url: u, prior, ua: BROWSER_UA });
        }
    }
    out
}

fn find_manifest(page: &str, html: &str) -> Option<String> {
    Regex::new(r"(?is)<link\b[^>]*>").unwrap().find_iter(html).find_map(|m| {
        let tag = m.as_str();
        attr(tag, "rel").is_some_and(|r| r.eq_ignore_ascii_case("manifest")).then(|| attr(tag, "href").and_then(|h| join_url(page, &h))).flatten()
    })
}

pub fn parse_manifest(url: &str, text: &str) -> Vec<Lead> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else { return Vec::new() };
    let mut icons: Vec<(u32, String, bool)> = v["icons"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| {
            let src = join_url(url, i["src"].as_str()?)?;
            let size = i["sizes"].as_str().and_then(|s| s.split_whitespace().filter_map(|p| p.split(['x', 'X']).next()?.parse::<u32>().ok()).max()).unwrap_or(0);
            let maskable = i["purpose"].as_str().is_some_and(|p| p.contains("maskable") && !p.contains("any"));
            Some((size, src, maskable))
        })
        .collect();
    icons.sort_by_key(|(s, _, m)| (*m, std::cmp::Reverse(*s)));
    icons
        .into_iter()
        .take(2)
        .map(|(size, src, maskable)| Lead {
            source: SourceKind::Website,
            label: format!("web manifest {size}px{}", if maskable { " (maskable)" } else { "" }),
            url: src,
            prior: if maskable { 0.6 } else { 0.76 },
            ua: BROWSER_UA,
        })
        .collect()
}

/// Bing image results whose title or URL mentions the product.
pub fn image_search(name: &str) -> Result<Vec<Lead>> {
    let q = format!("{name} logo icon");
    let html = get_text(&format!("https://www.bing.com/images/search?q={}&form=HDRSC2&first=1", url_encode(&q)), BROWSER_UA)?;
    Ok(parse_bing(name, &html))
}

pub fn parse_bing(name: &str, html: &str) -> Vec<Lead> {
    let mut out: Vec<Lead> = Vec::new();
    for c in Regex::new(r#"class="iusc"[^>]*?\sm="([^"]*)""#).unwrap().captures_iter(html) {
        let Ok(meta) = serde_json::from_str::<serde_json::Value>(&html_unescape(&c[1])) else { continue };
        let murl = meta["murl"].as_str().unwrap_or("").to_string();
        let title = meta["t"].as_str().unwrap_or("").to_string();
        let page = meta["purl"].as_str().unwrap_or("");
        if murl.is_empty() || !relevant(name, &[&title, &murl, page]) || out.iter().any(|l| l.url == murl) {
            continue;
        }
        let rank = out.len() as f32;
        let prior = 0.42 - rank * 0.01 + image_result_cues(&title, &murl);
        out.push(Lead { source: SourceKind::ImageSearch, label: if title.is_empty() { "image search".into() } else { title }, url: murl, prior, ua: BROWSER_UA });
        if out.len() >= 12 {
            break;
        }
    }
    out
}

/// Prior adjustment for an image result from its title and URL: store tiles and
/// files named like logos are good, screenshots and articles are not.
fn image_result_cues(title: &str, url: &str) -> f32 {
    static GOOD: OnceLock<Regex> = OnceLock::new();
    static BAD: OnceLock<Regex> = OnceLock::new();
    let u = url.to_lowercase();
    let file = u.split(['?', '#']).next().unwrap_or("").rsplit('/').next().unwrap_or("");
    let mut d = 0.0;
    if u.contains("store-images.s-microsoft.com") || u.contains("is1-ssl.mzstatic.com") || u.contains("play-lh.googleusercontent.com") {
        d += 0.3;
    }
    if crate::intake::re(&GOOD, r"(?i)logo|icon|favicon|brand|symbol").is_match(file) {
        d += 0.1;
    } else if crate::intake::re(&GOOD, r"(?i)logo|icon|favicon|brand|symbol").is_match(title) {
        d += 0.05;
    }
    let bad = crate::intake::re(
        &BAD,
        r"(?i)screen ?shot|setup|install|tutorial|how to|guide|manual|configur|settings|review|price|buy|download|allowing|using|wallpaper|banner|webinar|dashboard|step|error|vs\.?\s|comparison",
    );
    if bad.is_match(title) || bad.is_match(file) {
        d -= 0.15;
    }
    d
}

pub fn fetch_lead(lead: &Lead) -> Result<Candidate> {
    let (bytes, ctype) = get(&lead.url, lead.ua, 8 * 1024 * 1024)?;
    if ctype.starts_with("text/html") && !looks_like_svg(&bytes) {
        bail!("{} returned a web page, not an image", lead.url);
    }
    let d = decode(&bytes).with_context(|| lead.url.clone())?;
    Ok(finish(lead.source, lead.label.clone(), lead.url.clone(), d, lead.prior))
}

/// Runs every source in the background; candidates stream back as they decode,
/// calling `wake` after each event.
pub fn search(query: IconQuery, cancel: Arc<AtomicBool>, wake: Option<crate::process::Waker>) -> Receiver<SearchEvent> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let (inner_tx, inner_rx) = channel();
        let fwd = std::thread::spawn(move || {
            for ev in inner_rx {
                if tx.send(ev).is_err() {
                    break;
                }
                if let Some(w) = &wake {
                    w();
                }
            }
            let _ = tx.send(SearchEvent::Finished);
            if let Some(w) = &wake {
                w();
            }
        });
        run_search(&query, &cancel, &inner_tx);
        drop(inner_tx);
        let _ = fwd.join();
    });
    rx
}

fn run_search(q: &IconQuery, cancel: &AtomicBool, tx: &Sender<SearchEvent>) {
    let status = |s: String| {
        let _ = tx.send(SearchEvent::Status(s));
    };
    for f in &q.files {
        match from_file(f) {
            Ok(cands) => {
                status(format!("{}: {} icon(s)", f.file_name().unwrap_or_default().to_string_lossy(), cands.len()));
                for c in cands {
                    let _ = tx.send(SearchEvent::Candidate(Box::new(c)));
                }
            }
            Err(e) => status(format!("{e:#}")),
        }
    }
    if q.offline || q.name.trim().is_empty() || cancel.load(Ordering::Relaxed) {
        return;
    }
    let name = q.name.trim().to_string();
    let (lead_tx, lead_rx) = channel::<Lead>();
    let lead_rx = Mutex::new(lead_rx);
    let seen = Mutex::new(HashSet::<String>::new());
    let sites_done = Mutex::new(HashSet::<String>::new());
    let push_fn = |leads: Vec<Lead>, lead_tx: &Sender<Lead>| {
        for l in leads {
            if seen.lock().unwrap().insert(l.url.clone()) {
                let _ = lead_tx.send(l);
            }
        }
    };
    let site_fn = |url: String, lead_tx: Sender<Lead>| {
        let Some(host) = crate::intake::host_of(&url) else { return };
        if !sites_done.lock().unwrap().insert(host.clone()) || cancel.load(Ordering::Relaxed) {
            return;
        }
        match website(&url) {
            Ok(l) => {
                status(format!("Vendor site {host}: {} lead(s)", l.len()));
                push_fn(l, &lead_tx);
            }
            Err(e) => status(format!("Vendor site {host}: {e:#}")),
        }
    };
    std::thread::scope(|s| {
        let status = &status;
        let push = &push_fn;
        let site = &site_fn;
        let (n1, t1) = (name.clone(), lead_tx.clone());
        s.spawn(move || match wikidata(&n1) {
            Ok((leads, sites)) => {
                status(format!("Wikidata: {} image(s)", leads.len()));
                push(leads, &t1);
                for u in sites {
                    site(u, t1.clone());
                }
            }
            Err(e) => status(format!("Wikidata: {e:#}")),
        });
        let (n2, t2) = (name.clone(), lead_tx.clone());
        s.spawn(move || match chocolatey(&n2) {
            Ok((leads, sites)) => {
                status(format!("Chocolatey: {} icon(s)", leads.len()));
                push(leads, &t2);
                if q.vendor_url.is_empty() {
                    for u in sites.into_iter().take(1) {
                        site(u, t2.clone());
                    }
                }
            }
            Err(e) => status(format!("Chocolatey: {e:#}")),
        });
        if !q.vendor_url.trim().is_empty() {
            let (u, t3) = (q.vendor_url.trim().to_string(), lead_tx.clone());
            s.spawn(move || site(u, t3));
        }
        let (n4, t4) = (name.clone(), lead_tx.clone());
        s.spawn(move || match image_search(&n4) {
            Ok(leads) => {
                status(format!("Image search: {} relevant result(s)", leads.len()));
                push(leads, &t4);
            }
            Err(e) => status(format!("Image search: {e:#}")),
        });
        drop(lead_tx);
        for _ in 0..8 {
            s.spawn(|| {
                loop {
                    let next = lead_rx.lock().unwrap().recv();
                    let Ok(lead) = next else { break };
                    if cancel.load(Ordering::Relaxed) {
                        continue;
                    }
                    if let Ok(c) = fetch_lead(&lead) {
                        let _ = tx.send(SearchEvent::Candidate(Box::new(c)));
                    }
                }
            });
        }
    });
}

/// Blocking search that returns the merged, ranked candidates.
pub fn search_all(query: IconQuery) -> (Vec<Candidate>, Vec<String>) {
    let rx = search(query, Arc::new(AtomicBool::new(false)), None);
    let mut list = Vec::new();
    let mut log = Vec::new();
    for ev in rx {
        match ev {
            SearchEvent::Status(s) => log.push(s),
            SearchEvent::Candidate(c) => merge(&mut list, *c),
            SearchEvent::Finished => break,
        }
    }
    (list, log)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tile(bg: [u8; 4], fg: [u8; 4], size: u32, inset: u32) -> RgbaImage {
        RgbaImage::from_fn(size, size, |x, y| if x >= inset && y >= inset && x < size - inset && y < size - inset { Rgba(fg) } else { Rgba(bg) })
    }

    #[test]
    fn flood_fill_keeps_interior_background_colour() {
        // White background, black ring, white centre: only the outside goes.
        let mut img = RgbaImage::from_fn(40, 40, |x, y| {
            let (dx, dy) = (x as i32 - 20, y as i32 - 20);
            let r2 = dx * dx + dy * dy;
            if (100..=225).contains(&r2) { Rgba([0, 0, 0, 255]) } else { Rgba([255, 255, 255, 255]) }
        });
        remove_background(&mut img, 24);
        assert_eq!(img.get_pixel(0, 0)[3], 0);
        assert_eq!(img.get_pixel(20, 20)[3], 255, "white inside the ring must survive");
        assert_eq!(img.get_pixel(20, 7)[3], 255);
    }

    #[test]
    fn trim_square_and_resize_without_dark_fringe() {
        // Red 60x20 bar on a transparent 100x100 canvas.
        let img = RgbaImage::from_fn(100, 100, |x, y| if (20..80).contains(&x) && (40..60).contains(&y) { Rgba([255, 0, 0, 255]) } else { Rgba([0, 0, 0, 0]) });
        let out = process(&img, &Process { max_side: 30, ..Default::default() });
        assert_eq!(out.dimensions(), (30, 30));
        // Edge pixels are red with partial alpha, never darkened towards black.
        for p in out.pixels().filter(|p| p[3] > 0) {
            assert!(p[0] >= 250 && p[1] <= 5, "{p:?}");
        }
        assert_eq!(out.get_pixel(0, 0)[3], 0);
    }

    #[test]
    fn auto_detects_flat_backgrounds() {
        assert!(Process::auto(&tile([255, 255, 255, 255], [10, 80, 200, 255], 64, 10)).remove_background);
        assert!(!Process::auto(&tile([0, 0, 0, 0], [10, 80, 200, 255], 64, 10)).remove_background);
    }

    #[test]
    fn encode_respects_limits() {
        let big = RgbaImage::from_fn(900, 900, |x, y| Rgba([(x * 7 % 256) as u8, (y * 13 % 256) as u8, ((x ^ y) % 256) as u8, 255]));
        let bytes = encode_png(&big).unwrap();
        let back = image::load_from_memory(&bytes).unwrap();
        assert!(back.width() <= MAX_SIDE && bytes.len() <= MAX_BYTES);
    }

    #[test]
    fn scoring_prefers_square_transparent_large() {
        let good = finish(SourceKind::Website, "a".into(), "a".into(), Decoded { image: tile([0, 0, 0, 0], [0, 120, 255, 255], 256, 16), native: (256, 256), vector: false }, 0.7);
        let wide = finish(
            SourceKind::Website,
            "b".into(),
            "b".into(),
            Decoded { image: RgbaImage::from_pixel(300, 80, Rgba([0, 120, 255, 255])), native: (300, 80), vector: false },
            0.7,
        );
        assert!(good.score > wide.score + 0.3, "{} vs {}", good.score, wide.score);
        let mut list = vec![];
        merge(&mut list, wide);
        merge(&mut list, good.clone());
        let mut dup = good.clone();
        dup.source = SourceKind::Chocolatey;
        dup.origin = "c".into();
        merge(&mut list, dup);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].also_from.len(), 1);
    }

    #[test]
    fn decodes_svg_and_ico() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><rect x="1" y="1" width="8" height="8" fill="#f00"/></svg>"##;
        let d = decode(svg).unwrap();
        assert!(d.vector && d.image.width() == MAX_SIDE);
        assert_eq!(d.image.get_pixel(256, 256)[0], 255);
        // An ICO written by the image crate round-trips at its real size.
        let mut ico = Vec::new();
        image::DynamicImage::ImageRgba8(tile([0, 0, 0, 0], [0, 200, 0, 255], 48, 4)).write_to(&mut std::io::Cursor::new(&mut ico), image::ImageFormat::Ico).unwrap();
        assert_eq!(decode(&ico).unwrap().native, (48, 48));
    }

    #[test]
    fn extracts_true_size_exe_icon() {
        let exe = Path::new(r"C:\Windows\explorer.exe");
        if !exe.is_file() {
            return;
        }
        let c = from_file(exe).unwrap();
        assert!(!c.is_empty());
        assert!(c[0].native.0 >= 128, "{:?}", c[0].native);
    }

    #[test]
    fn parses_page_icons_manifest_and_urls() {
        let html = r#"<html><head><link rel="icon" href="/favicon-32.png" sizes="32x32"><link rel="apple-touch-icon" href="img/touch.png" sizes="180x180"><link rel="manifest" href="/site.webmanifest"><meta property="og:image" content="https://cdn.x.com/og.jpg"></head></html>"#;
        let leads = parse_html_icons("https://x.com/products/a", html);
        assert!(leads.iter().any(|l| l.url == "https://x.com/products/img/touch.png" && l.prior > 0.7));
        assert!(leads.iter().any(|l| l.url == "https://x.com/favicon-32.png" && l.prior < 0.6));
        assert!(leads.iter().any(|l| l.url == "https://cdn.x.com/og.jpg" && l.prior < 0.4));
        assert_eq!(find_manifest("https://x.com/", html).unwrap(), "https://x.com/site.webmanifest");
        let m = parse_manifest("https://x.com/site.webmanifest", r#"{"icons":[{"src":"/a-192.png","sizes":"192x192"},{"src":"/a-512.png","sizes":"512x512","purpose":"any"},{"src":"/m.png","sizes":"512x512","purpose":"maskable"}]}"#);
        assert_eq!(m[0].url, "https://x.com/a-512.png");
        assert_eq!(join_url("https://a.com/x/y", "//cdn.b.com/i.png").unwrap(), "https://cdn.b.com/i.png");
        assert_eq!(url_encode("a b&c"), "a%20b%26c");
    }

    #[test]
    fn bing_results_are_filtered_for_relevance() {
        let m1 = html_escape_json(r#"{"murl":"https://extron.com/pcs.png","t":"PCS - Software | Extron","purl":"https://extron.com/pcs"}"#);
        let m2 = html_escape_json(r#"{"murl":"https://pagesix.com/swift.jpg","t":"Taylor Swift and Joe Alwyn","purl":"https://pagesix.com/x"}"#);
        let html = format!(r#"<a class="iusc" style="" m="{m1}" h="x"></a><a class="iusc" m="{m2}"></a>"#);
        let leads = parse_bing("Extron PCS", &html);
        assert_eq!(leads.len(), 1);
        assert_eq!(leads[0].url, "https://extron.com/pcs.png");
    }

    fn html_escape_json(s: &str) -> String {
        s.replace('&', "&amp;").replace('"', "&quot;")
    }

    #[test]
    fn chocolatey_feed_and_rawgit() {
        let xml = r#"<feed><entry><id>https://community.chocolatey.org/api/v2/Packages(Id='qgis',Version='3.40')</id><m:properties><d:Title>QGIS</d:Title><d:IconUrl>https://cdn.rawgit.com/teknowledgist/Chocolatey-packages/master/Icons/qgis_icon.png</d:IconUrl><d:ProjectUrl>https://www.qgis.org/</d:ProjectUrl></m:properties></entry><entry><d:Title>Notepad++</d:Title><d:IconUrl>https://x/npp.png</d:IconUrl></entry></feed>"#;
        let (leads, sites) = parse_chocolatey("QGIS", xml);
        assert_eq!(leads.len(), 1);
        assert_eq!(leads[0].url, "https://cdn.jsdelivr.net/gh/teknowledgist/Chocolatey-packages@master/Icons/qgis_icon.png");
        assert_eq!(sites, vec!["https://www.qgis.org/"]);
    }

    #[test]
    fn icon_names() {
        assert_eq!(icon_file_name("Notepad++"), "notepadplusplus.png");
        assert_eq!(icon_file_name("qgis.png"), "qgis.png");
    }

    #[test]
    #[ignore = "network"]
    fn live_search_finds_qgis() {
        let names = std::env::var("YS_ICON").unwrap_or_else(|_| "QGIS|https://qgis.org/download/".into());
        let out = std::env::temp_dir().join("ystudio-icon-live");
        let _ = std::fs::create_dir_all(&out);
        for spec in names.split(';') {
            let (name, url) = spec.split_once('|').unwrap_or((spec, ""));
            let t = std::time::Instant::now();
            let (list, log) = search_all(IconQuery { name: name.into(), vendor_url: url.into(), ..Default::default() });
            eprintln!("=== {name} ({:.1}s) {log:?}", t.elapsed().as_secs_f32());
            for (i, c) in list.iter().take(6).enumerate() {
                eprintln!("  {:.2} {:?} {} {:?} {}\n       {:?}", c.score, c.source, c.label, c.native, c.origin, c.reasons);
                let img = process(&c.image, &Process::auto(&c.image));
                let _ = save_png(&img, &out.join(format!("{}-{i}.png", icon_file_name(name).trim_end_matches(".png"))));
            }
            assert!(!list.is_empty(), "{name}: nothing found");
        }
    }
}
