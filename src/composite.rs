//! Builds the final photo grid from the captured shots (1, 2, or 4 panels).
//!
//! An optional caption can be printed across the bottom border via the
//! environment:
//!
//! - `PHOTOBOOTH_BANNER_TEXT` — the text to render (unset/blank ⇒ no caption).
//! - `PHOTOBOOTH_BANNER_FONT_SIZE` — pixel size (default `48`).
//! - `PHOTOBOOTH_BANNER_FONT` — path to a `.ttf`/`.otf` font file (defaults to
//!   the bundled Tangerine font).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ab_glyph::{Font, FontVec, PxScale, ScaleFont, point};
use image::{Rgba, RgbaImage, imageops};

/// Width each captured shot is scaled to inside the grid.
const PANEL_W: u32 = 1000;
/// White border between adjacent panels (the interior border).
const INNER_BORDER: u32 = 24;
/// White border around the edge of the canvas.
const OUTER_BORDER: u32 = 100;
/// Bounding box the banner is scaled to fit (preserving aspect ratio).
const BANNER_BOX: u32 = 400;
/// Canvas background colour (the "paper" behind the photos).
const BG: Rgba<u8> = Rgba([250, 250, 250, 255]);

/// Banner overlaid onto the bottom-left of the finished grid, embedded at build
/// time so it travels with the binary regardless of the working directory.
const BANNER_PNG: &[u8] = include_bytes!("../assets/banner.png");

/// Default banner-text size when `PHOTOBOOTH_BANNER_FONT_SIZE` is unset.
const DEFAULT_FONT_SIZE: f32 = 80.0;
/// Default caption font, embedded at build time so it always ships with the
/// binary. Overridden by a `.ttf`/`.otf` path in `PHOTOBOOTH_BANNER_FONT`.
const DEFAULT_FONT_TTF: &[u8] = include_bytes!("../assets/Tangerine-Regular.ttf");

/// Arrange the shots onto a solid background, laid out by [`grid_dims`] for the
/// shot count (1 single panel, 2 vertically stacked, 4 in a 2x2 grid).
///
/// The panel height is derived from the first shot's aspect ratio; every DSLR
/// frame in a session shares the same ratio, so the grid stays uniform.
pub fn build(shots: &[RgbaImage]) -> RgbaImage {
    let (cols, rows) = grid_dims(shots.len());
    let (sw, sh) = shots
        .first()
        .map(|s| (s.width().max(1), s.height().max(1)))
        .unwrap_or((3, 2));
    let panel_h = ((u64::from(PANEL_W) * u64::from(sh)) / u64::from(sw)) as u32;

    // Two outer borders plus one interior border between each row/column.
    let canvas_w = OUTER_BORDER * 2 + PANEL_W * cols + INNER_BORDER * (cols - 1);
    let canvas_h = OUTER_BORDER * 2 + panel_h * rows + INNER_BORDER * (rows - 1);
    let mut canvas = RgbaImage::from_pixel(canvas_w, canvas_h, BG);

    for (i, shot) in shots.iter().take((cols * rows) as usize).enumerate() {
        let resized = imageops::resize(shot, PANEL_W, panel_h, imageops::FilterType::Triangle);
        let col = i as u32 % cols;
        let row = i as u32 / cols;
        let x = OUTER_BORDER + col * (PANEL_W + INNER_BORDER);
        let y = OUTER_BORDER + row * (panel_h + INNER_BORDER);
        imageops::overlay(&mut canvas, &resized, i64::from(x), i64::from(y));
    }

    overlay_banner(&mut canvas);
    overlay_banner_text(&mut canvas);

    canvas
}

/// Grid layout (columns, rows) for a given shot count: 1 ⇒ 1x1, 2 ⇒ 1x2
/// (vertically stacked), 4 ⇒ 2x2. Any other count falls back to a near-square
/// grid.
fn grid_dims(count: usize) -> (u32, u32) {
    match count {
        0 | 1 => (1, 1),
        2 => (1, 2),
        4 => (2, 2),
        n => {
            let cols = (n as f64).sqrt().ceil() as u32;
            let rows = (n as u32).div_ceil(cols);
            (cols, rows)
        }
    }
}

/// Render the composite *template* — solid black boxes in place of the photos —
/// for the given shot `count` (1, 2, or 4) and save it to
/// `./composite-template.jpg`. Lets you iterate on the layout (borders, banner,
/// caption) without a camera. Returns the saved path.
pub fn render_template(count: usize) -> Result<PathBuf, String> {
    // A 3:2 black box stands in for each DSLR frame.
    let black = RgbaImage::from_pixel(1500, 1000, Rgba([0, 0, 0, 255]));
    let shots: Vec<RgbaImage> = vec![black; count.max(1)];
    let grid = build(&shots);
    let path = PathBuf::from("composite-template.jpg");
    encode_jpg(&path, &grid)?;
    Ok(path)
}

/// Composite the embedded banner onto the bottom-left corner of the canvas,
/// flush to the edge so it sits over the outer white border.
fn overlay_banner(canvas: &mut RgbaImage) {
    let banner = match image::load_from_memory(BANNER_PNG) {
        Ok(img) => img
            .resize(BANNER_BOX, BANNER_BOX, imageops::FilterType::Triangle)
            .into_rgba8(),
        Err(e) => {
            eprintln!("skip banner overlay: {e}");
            return;
        }
    };
    let x = 0;
    let y = canvas.height().saturating_sub(banner.height());
    imageops::overlay(canvas, &banner, i64::from(x), i64::from(y));
}

/// Optional caption rendered in black across the bottom outer border. Configured
/// entirely from the environment (see the module docs).
struct BannerText {
    text: String,
    font_size: f32,
    /// Override font path; `None` uses the embedded [`DEFAULT_FONT_TTF`].
    font_path: Option<String>,
}

impl BannerText {
    /// Read the caption config from the environment; `None` when no text is set.
    fn from_env() -> Option<Self> {
        let text = std::env::var("PHOTOBOOTH_BANNER_TEXT").ok()?;
        if text.trim().is_empty() {
            return None;
        }
        let font_size = std::env::var("PHOTOBOOTH_BANNER_FONT_SIZE")
            .ok()
            .and_then(|s| s.trim().parse::<f32>().ok())
            .filter(|s| *s > 0.0)
            .unwrap_or(DEFAULT_FONT_SIZE);
        let font_path = std::env::var("PHOTOBOOTH_BANNER_FONT")
            .ok()
            .filter(|s| !s.trim().is_empty());
        Some(Self {
            text,
            font_size,
            font_path,
        })
    }

    /// Load the configured font: the override path, or the embedded default.
    fn load_font(&self) -> Result<FontVec, String> {
        match &self.font_path {
            Some(path) => {
                let bytes = std::fs::read(path).map_err(|e| format!("read {path}: {e}"))?;
                FontVec::try_from_vec(bytes).map_err(|e| format!("parse {path}: {e}"))
            }
            None => FontVec::try_from_vec(DEFAULT_FONT_TTF.to_vec())
                .map_err(|e| format!("parse bundled font: {e}")),
        }
    }
}

/// Draw the configured caption in black, horizontally centred and vertically
/// centred within the bottom outer-border band. No-op when unconfigured; on any
/// failure (missing/invalid font) it logs and leaves the image untouched.
fn overlay_banner_text(canvas: &mut RgbaImage) {
    let Some(cfg) = BannerText::from_env() else {
        return;
    };
    let font = match cfg.load_font() {
        Ok(font) => font,
        Err(e) => {
            eprintln!("skip banner text: {e}");
            return;
        }
    };

    let scale = PxScale::from(cfg.font_size);
    let scaled = font.as_scaled(scale);

    // Lay the glyphs out along a provisional baseline (caret advances with
    // kerning), keeping the rasterisable ones to position in a second pass.
    let mut outlines = Vec::new();
    let mut caret = 0.0f32;
    let mut prev = None;
    for c in cfg.text.chars() {
        let id = font.glyph_id(c);
        if let Some(p) = prev {
            caret += scaled.kern(p, id);
        }
        let glyph = id.with_scale_and_position(scale, point(caret, 0.0));
        caret += scaled.h_advance(id);
        prev = Some(id);
        if let Some(outline) = font.outline_glyph(glyph) {
            outlines.push(outline);
        }
    }

    // Centre on the actual ink bounds rather than font metrics — decorative
    // faces (e.g. Tangerine) carry huge ascent/descent that would otherwise
    // throw the text off the canvas.
    let Some(ink) = ink_bounds(&outlines) else {
        return; // nothing visible (e.g. all whitespace)
    };
    let canvas_w = canvas.width() as f32;
    let band_center_y = canvas.height() as f32 - OUTER_BORDER as f32 / 2.0;
    let offset_x = (canvas_w - (ink.max.x - ink.min.x)) / 2.0 - ink.min.x;
    let offset_y = band_center_y - (ink.max.y - ink.min.y) / 2.0 - ink.min.y;

    let (cw, ch) = (canvas.width(), canvas.height());
    for outline in &outlines {
        let bounds = outline.px_bounds();
        outline.draw(|gx, gy, coverage| {
            let px = bounds.min.x + offset_x + gx as f32;
            let py = bounds.min.y + offset_y + gy as f32;
            if px < 0.0 || py < 0.0 {
                return;
            }
            let (px, py) = (px as u32, py as u32);
            if px < cw && py < ch {
                blend_black(canvas.get_pixel_mut(px, py), coverage);
            }
        });
    }
}

/// Union of the pixel bounding boxes of every glyph, i.e. the text's ink
/// extent. `None` when there are no drawable glyphs.
fn ink_bounds(outlines: &[ab_glyph::OutlinedGlyph]) -> Option<ab_glyph::Rect> {
    outlines
        .iter()
        .map(ab_glyph::OutlinedGlyph::px_bounds)
        .reduce(|a, b| ab_glyph::Rect {
            min: point(a.min.x.min(b.min.x), a.min.y.min(b.min.y)),
            max: point(a.max.x.max(b.max.x), a.max.y.max(b.max.y)),
        })
}

/// Alpha-blend a black pixel over `px` at the given glyph coverage (0..=1).
fn blend_black(px: &mut Rgba<u8>, coverage: f32) {
    let inv = 1.0 - coverage.clamp(0.0, 1.0);
    px[0] = (px[0] as f32 * inv) as u8;
    px[1] = (px[1] as f32 * inv) as u8;
    px[2] = (px[2] as f32 * inv) as u8;
}

/// Create a fresh timestamped session directory, `./captures/photobooth-<ts>/`,
/// where this session's individual shots and composite are saved.
pub fn new_session_dir() -> Result<PathBuf, String> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dir = PathBuf::from("captures").join(format!("photobooth-{ts}"));
    std::fs::create_dir_all(&dir).map_err(|e| format!("create session dir: {e}"))?;
    Ok(dir)
}

/// Save an original camera capture (raw file bytes) as `shot-<n>.<ext>` in `dir`.
pub fn save_shot(dir: &Path, index: usize, bytes: &[u8], ext: &str) -> Result<PathBuf, String> {
    let path = dir.join(format!("shot-{index}.{ext}"));
    std::fs::write(&path, bytes).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}

/// Save the finished grid as `composite.jpg` in `dir`.
///
/// JPEG (not PNG) so the macOS iMessage sender can attach it directly with no
/// conversion step. JPEG has no alpha channel, so it's dropped before encoding.
pub fn save_composite(dir: &Path, image: &RgbaImage) -> Result<PathBuf, String> {
    let path = dir.join("composite.jpg");
    encode_jpg(&path, image)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::{build, grid_dims};
    use image::{Rgba, RgbaImage};

    #[test]
    fn grid_dims_for_supported_counts() {
        assert_eq!(grid_dims(1), (1, 1));
        assert_eq!(grid_dims(2), (1, 2));
        assert_eq!(grid_dims(4), (2, 2));
    }

    #[test]
    fn build_lays_out_each_count() {
        // Canvas dimensions should grow with the column/row count for each
        // supported layout, and building must never panic.
        let panel = RgbaImage::from_pixel(1500, 1000, Rgba([0, 0, 0, 255]));
        let make = |n: usize| build(&vec![panel.clone(); n]);

        let one = make(1);
        let two = make(2);
        let four = make(4);

        // 2 photos are vertically stacked: same width as 1, taller.
        assert_eq!(two.width(), one.width());
        assert!(two.height() > one.height());
        // 4 photos add a second column: wider than 2, same height.
        assert!(four.width() > two.width());
        assert_eq!(four.height(), two.height());
    }
}

/// Encode `image` to `path` as JPEG (alpha dropped, quality 88).
fn encode_jpg(path: &Path, image: &RgbaImage) -> Result<(), String> {
    let rgb = image::DynamicImage::ImageRgba8(image.clone()).into_rgb8();
    let mut file =
        std::fs::File::create(path).map_err(|e| format!("create {}: {e}", path.display()))?;
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut file, 88)
        .encode_image(&rgb)
        .map_err(|e| format!("encode jpg: {e}"))
}
