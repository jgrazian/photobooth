//! Builds the final 2x2 photo grid from the four captured shots.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use image::{Rgba, RgbaImage, imageops};

/// Width each captured shot is scaled to inside the grid.
const PANEL_W: u32 = 1000;
/// Padding between panels and around the edge of the canvas.
const GAP: u32 = 24;
/// Canvas background colour (the "paper" behind the photos).
const BG: Rgba<u8> = Rgba([250, 250, 250, 255]);

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

    let canvas_w = PANEL_W * 2 + GAP * 3;
    let canvas_h = panel_h * 2 + GAP * 3;
    let mut canvas = RgbaImage::from_pixel(canvas_w, canvas_h, BG);

    for (i, shot) in shots.iter().take(4).enumerate() {
        let resized = imageops::resize(shot, PANEL_W, panel_h, imageops::FilterType::Triangle);
        let col = (i % 2) as u32;
        let row = (i / 2) as u32;
        let x = GAP + col * (PANEL_W + GAP);
        let y = GAP + row * (panel_h + GAP);
        imageops::overlay(&mut canvas, &resized, i64::from(x), i64::from(y));
    }

    canvas
}

/// Save the composite under `./captures/photobooth-<unix-secs>.png`.
pub fn save(image: &RgbaImage) -> Result<PathBuf, String> {
    let dir = PathBuf::from("captures");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create captures dir: {e}"))?;

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = dir.join(format!("photobooth-{ts}.png"));

    image.save(&path).map_err(|e| format!("save png: {e}"))?;
    Ok(path)
}
