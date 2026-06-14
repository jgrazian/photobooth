//! Builds the final 2x2 photo grid from the four captured shots.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use image::{Rgba, RgbaImage, imageops};

/// Width each captured shot is scaled to inside the grid.
const PANEL_W: u32 = 1000;
/// White border between adjacent panels (the interior border).
const INNER_BORDER: u32 = 24;
/// White border around the edge of the canvas.
const OUTER_BORDER: u32 = 100;
/// Bounding box the banner is scaled to fit (preserving aspect ratio).
const BANNER_BOX: u32 = 600;
/// Canvas background colour (the "paper" behind the photos).
const BG: Rgba<u8> = Rgba([250, 250, 250, 255]);

/// Banner overlaid onto the bottom-left of the finished grid, embedded at build
/// time so it travels with the binary regardless of the working directory.
const BANNER_PNG: &[u8] = include_bytes!("../assets/banner.png");

/// Arrange up to four shots into a 2x2 grid on a solid background.
///
/// The panel height is derived from the first shot's aspect ratio; every DSLR
/// frame in a session shares the same ratio, so the grid stays uniform.
pub fn build(shots: &[RgbaImage]) -> RgbaImage {
    let (sw, sh) = shots
        .first()
        .map(|s| (s.width().max(1), s.height().max(1)))
        .unwrap_or((3, 2));
    let panel_h = ((u64::from(PANEL_W) * u64::from(sh)) / u64::from(sw)) as u32;

    // Two outer borders plus one interior border across each axis.
    let canvas_w = OUTER_BORDER * 2 + PANEL_W * 2 + INNER_BORDER;
    let canvas_h = OUTER_BORDER * 2 + panel_h * 2 + INNER_BORDER;
    let mut canvas = RgbaImage::from_pixel(canvas_w, canvas_h, BG);

    for (i, shot) in shots.iter().take(4).enumerate() {
        let resized = imageops::resize(shot, PANEL_W, panel_h, imageops::FilterType::Triangle);
        let col = (i % 2) as u32;
        let row = (i / 2) as u32;
        let x = OUTER_BORDER + col * (PANEL_W + INNER_BORDER);
        let y = OUTER_BORDER + row * (panel_h + INNER_BORDER);
        imageops::overlay(&mut canvas, &resized, i64::from(x), i64::from(y));
    }

    overlay_banner(&mut canvas);

    canvas
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
    let rgb = image::DynamicImage::ImageRgba8(image.clone()).into_rgb8();
    let mut file =
        std::fs::File::create(&path).map_err(|e| format!("create {}: {e}", path.display()))?;
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut file, 88)
        .encode_image(&rgb)
        .map_err(|e| format!("encode jpg: {e}"))?;
    Ok(path)
}
