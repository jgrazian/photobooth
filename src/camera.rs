//! Camera worker: owns the gphoto2 context/camera on its own thread and drives
//! the live preview + capture sequence, reporting back to the UI by channel.
//!
//! gphoto2 calls are blocking, so they must never run on the egui thread. The
//! `gphoto2::Task` returned by each call has a blocking [`Task::wait`], which is
//! all we need here — no async runtime required.

use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use egui::{ColorImage, Context as EguiContext};
use gphoto2::widget::{RadioWidget, ToggleWidget, Widget};
use gphoto2::{Camera, Context};
use image::{RgbaImage, imageops};

use crate::composite;

/// Per-session capture settings chosen by the guest via the on-screen config
/// button: how many shots to take and how long the countdown runs before each.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SessionConfig {
    /// Number of shots taken per session (1, 2, or 4).
    pub shots: usize,
    /// Seconds the on-screen countdown runs before each shot (3, 5, or 7).
    pub countdown_secs: u64,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            shots: 4,
            countdown_secs: 5,
        }
    }
}

/// How long the just-captured photo is shown before the next countdown.
const REVIEW: Duration = Duration::from_millis(2000);
/// Max texture side for review/composite display images (keeps GPU happy).
const MAX_DISPLAY_SIDE: u32 = 1600;

/// Commands sent from the UI to the camera thread.
pub enum Cmd {
    /// Begin a photobooth session with the given shot count and timer.
    Start(SessionConfig),
    /// Resume idle live preview (e.g. after a finished set was sent).
    Preview,
    /// Shut the worker down (sent on app exit).
    Quit,
}

/// Events sent from the camera thread back to the UI.
pub enum Event {
    /// Camera connected; carries the model name.
    Connected(String),
    /// A fresh live-view frame.
    Preview(ColorImage),
    /// A full-resolution shot was captured (downscaled for display).
    Captured { index: usize, image: ColorImage },
    /// The finished grid, plus where it was saved.
    Composite {
        image: ColorImage,
        saved: Option<String>,
    },
    /// A change in the session state machine.
    Status(Status),
    /// Something went wrong; carries a human-readable message.
    Error(String),
}

/// Where we are in the session state machine.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// No session running; live preview is showing.
    Idle,
    /// Counting down to shot `shot` (0-based); `remaining` runs countdown_secs..=1.
    Countdown { shot: usize, remaining: u32 },
    /// Triggering the shutter for `shot`.
    Capturing { shot: usize },
    /// Showing the photo we just took.
    Review { shot: usize },
    /// Assembling the grid.
    Compositing,
    /// Session finished; the composite is on screen.
    Done,
}

/// Handle to the running camera worker.
pub struct Worker {
    /// Commands to the worker.
    pub cmd_tx: Sender<Cmd>,
    /// Events from the worker.
    pub evt_rx: Receiver<Event>,
    /// Join handle, used on shutdown to ensure the camera is released.
    pub join: thread::JoinHandle<()>,
}

/// Spawn the camera worker thread and return a handle to talk to it.
pub fn spawn(egui_ctx: EguiContext) -> Worker {
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (evt_tx, evt_rx) = mpsc::channel();
    let join = thread::spawn(move || run(egui_ctx, cmd_rx, evt_tx));
    Worker {
        cmd_tx,
        evt_rx,
        join,
    }
}

fn run(ctx: EguiContext, cmd_rx: Receiver<Cmd>, tx: Sender<Event>) {
    let context = match Context::new() {
        Ok(c) => c,
        Err(e) => return fail(&ctx, &tx, format!("Failed to initialise gphoto2: {e}")),
    };
    let camera = match context.autodetect_camera().wait() {
        Ok(c) => c,
        Err(e) => {
            return fail(
                &ctx,
                &tx,
                format!("No camera detected ({e}). Is it on and connected?"),
            );
        }
    };

    // Trade resolution for speed: a photobooth grid only needs ~1000px panels,
    // so a small Basic JPEG transfers far faster than a full-res frame. Capturing
    // to internal RAM also skips the (slow) SD-card write. All best-effort —
    // unsupported keys are simply ignored.
    configure_for_speed(&camera);
    log_config_tree(&camera);

    let model = camera.abilities().model().to_string();
    eprintln!("[camera] connected: {model}");
    let _ = tx.send(Event::Connected(model));
    let _ = tx.send(Event::Status(Status::Idle));
    ctx.request_repaint();

    // While `streaming`, we continuously push live-view frames so the user can
    // frame themselves. It's switched off once a session completes, leaving the
    // finished composite on screen (and the camera's mirror down) until "Begin".
    let mut streaming = true;

    loop {
        match cmd_rx.try_recv() {
            Ok(Cmd::Start(cfg)) => {
                run_session(&ctx, &camera, &context, &tx, cfg);
                streaming = false;
            }
            Ok(Cmd::Preview) => {
                streaming = true;
                let _ = tx.send(Event::Status(Status::Idle));
                ctx.request_repaint();
            }
            Ok(Cmd::Quit) | Err(TryRecvError::Disconnected) => return,
            Err(TryRecvError::Empty) => {}
        }

        if streaming {
            match grab_preview(&camera, &context) {
                Ok(img) => {
                    let _ = tx.send(Event::Preview(img));
                    ctx.request_repaint();
                }
                Err(e) => {
                    let _ = tx.send(Event::Error(e));
                    ctx.request_repaint();
                    thread::sleep(Duration::from_millis(500));
                }
            }
        } else {
            thread::sleep(Duration::from_millis(40));
        }
    }
}

/// Run one full session (`cfg.shots` shots): countdown, capture, review, then
/// composite.
fn run_session(
    ctx: &EguiContext,
    camera: &Camera,
    context: &Context,
    tx: &Sender<Event>,
    cfg: SessionConfig,
) {
    let mut shots: Vec<RgbaImage> = Vec::with_capacity(cfg.shots);

    // Per-session folder for the individual shots and the composite. Best-effort:
    // if it can't be created we still run the booth, just without saving.
    let session_dir = match composite::new_session_dir() {
        Ok(dir) => Some(dir),
        Err(e) => {
            eprintln!("[save] {e}");
            None
        }
    };

    // Prime autofocus once when the shoot begins (live view must be active first).
    if let Ok(img) = grab_preview(camera, context) {
        let _ = tx.send(Event::Preview(img));
        ctx.request_repaint();
    }
    autofocus(camera);

    for shot in 0..cfg.shots {
        // Wake live view (it stops after each capture) so the countdown shows a
        // live feed and autofocus has an image to work with at capture time.
        if let Ok(img) = grab_preview(camera, context) {
            let _ = tx.send(Event::Preview(img));
            ctx.request_repaint();
        }

        // --- Countdown, with live preview running underneath ---
        let start = Instant::now();
        let total = Duration::from_secs(cfg.countdown_secs);
        let mut last_remaining = u32::MAX;
        loop {
            let elapsed = start.elapsed();
            if elapsed >= total {
                break;
            }
            let remaining = cfg.countdown_secs as u32 - elapsed.as_secs() as u32; // e.g. 5, 4, 3, 2, 1
            if remaining != last_remaining {
                last_remaining = remaining;
                let _ = tx.send(Event::Status(Status::Countdown { shot, remaining }));
            }
            if let Ok(img) = grab_preview(camera, context) {
                let _ = tx.send(Event::Preview(img));
            }
            ctx.request_repaint();
        }

        // --- Capture: refocus right before the shutter for a sharp frame ---
        let _ = tx.send(Event::Status(Status::Capturing { shot }));
        ctx.request_repaint();
        autofocus(camera);
        match capture_full(camera, context) {
            Ok(capture) => {
                let _ = tx.send(Event::Captured {
                    index: shot,
                    image: to_color_image(&downscale(&capture.image, MAX_DISPLAY_SIDE)),
                });
                // Save the original camera file (shot-1.jpg, shot-2.jpg, …).
                if let Some(dir) = &session_dir
                    && let Err(e) = composite::save_shot(dir, shot + 1, &capture.bytes, &capture.ext)
                {
                    eprintln!("[save] shot {}: {e}", shot + 1);
                }
                shots.push(capture.image);
                let _ = tx.send(Event::Status(Status::Review { shot }));
                ctx.request_repaint();
                thread::sleep(REVIEW);
            }
            Err(e) => {
                let _ = tx.send(Event::Error(e));
                ctx.request_repaint();
                return; // abort the session
            }
        }
    }

    // --- Composite ---
    let _ = tx.send(Event::Status(Status::Compositing));
    ctx.request_repaint();

    let grid = composite::build(&shots);
    let saved = session_dir.as_ref().and_then(|dir| {
        match composite::save_composite(dir, &grid) {
            Ok(path) => Some(path.display().to_string()),
            Err(e) => {
                let _ = tx.send(Event::Error(format!("Couldn't save photo: {e}")));
                None
            }
        }
    });
    let _ = tx.send(Event::Composite {
        image: to_color_image(&downscale(&grid, MAX_DISPLAY_SIDE)),
        saved,
    });
    let _ = tx.send(Event::Status(Status::Done));
    ctx.request_repaint();
}

/// Best-effort tweaks that make each shot transfer faster. Every step is
/// optional: cameras name these configs differently (or not at all), so a
/// missing key or rejected value is silently skipped.
fn configure_for_speed(camera: &Camera) {
    // Skip the SD-card write by capturing into the camera's memory.
    set_choice_matching(camera, "capturetarget", &["internal ram", "sdram", "ram"]);
    // Smallest JPEG the body offers — plenty for the composite.
    set_smallest_size(camera, "imagesize");
    // Lowest-quality JPEG (smallest file). Avoid RAW, which is huge.
    set_choice_matching(
        camera,
        "imagequality",
        &[
            "jpeg normal",
            "normal",
            "jpeg fine",
            "fine",
            "jpeg basic",
            "basic",
        ],
    );
}

/// Drive one autofocus cycle. On Nikon/PTP bodies the `autofocusdrive` toggle
/// runs contrast-detect AF while live view is active, so the subject is sharp
/// by the time the shutter fires. Best-effort and blocking: while the lens hunts
/// (a few hundred ms) the preview pauses on its last frame. It's fired once to
/// prime AF when the shoot begins, then again right before each shutter release.
fn autofocus(camera: &Camera) -> bool {
    // Diagnostics: if the body/lens is in manual focus, the drive is a no-op.
    log_radio_value(camera, "focusmode");
    log_radio_value(camera, "liveviewaffocus");

    let widget = match camera.config_key::<ToggleWidget>("autofocusdrive").wait() {
        Ok(w) => w,
        Err(e) => {
            eprintln!(
                "[af] no 'autofocusdrive' config on this camera ({e}). \
                 Run with PHOTOBOOTH_DEBUG=1 to list the real key names."
            );
            return false;
        }
    };
    eprintln!("[af] driving autofocus…");
    let start = Instant::now();
    widget.set_toggled(true);
    match camera.set_config(&widget).wait() {
        Ok(()) => {
            eprintln!(
                "[af] autofocus drive accepted ({} ms)",
                start.elapsed().as_millis()
            );
            true
        }
        Err(e) => {
            eprintln!("[af] autofocus drive rejected by camera: {e}");
            false
        }
    }
}

/// Log the current value of a radio/menu config, if present. Quietly skips keys
/// the camera doesn't expose.
fn log_radio_value(camera: &Camera, key: &str) {
    if let Ok(widget) = camera.config_key::<RadioWidget>(key).wait() {
        eprintln!("[af] {key} = {}", widget.choice());
    }
}

/// Dump every config key the camera exposes, when `PHOTOBOOTH_DEBUG` is set.
/// Use it to find the exact key names (autofocus, quality, …) for a given body.
fn log_config_tree(camera: &Camera) {
    if std::env::var_os("PHOTOBOOTH_DEBUG").is_none() {
        return;
    }
    match camera.config().wait() {
        Ok(root) => {
            eprintln!("[config] --- available settings ---");
            log_widget(&Widget::Group(root), 0);
        }
        Err(e) => eprintln!("[config] could not read config: {e}"),
    }
}

/// Recursively print one widget (and its children) with indentation.
fn log_widget(widget: &Widget, depth: usize) {
    let indent = "  ".repeat(depth);
    let name = widget.name();
    let ro = if widget.readonly() {
        " [read-only]"
    } else {
        ""
    };
    match widget {
        Widget::Group(group) => {
            eprintln!("[config] {indent}{name}/");
            for child in group.children_iter() {
                log_widget(&child, depth + 1);
            }
        }
        Widget::Toggle(_) => eprintln!("[config] {indent}{name} (toggle){ro}"),
        Widget::Radio(_) => eprintln!("[config] {indent}{name} (radio){ro}"),
        Widget::Text(_) => eprintln!("[config] {indent}{name} (text){ro}"),
        Widget::Range(_) => eprintln!("[config] {indent}{name} (range){ro}"),
        Widget::Button(_) => eprintln!("[config] {indent}{name} (button){ro}"),
        Widget::Date(_) => eprintln!("[config] {indent}{name} (date){ro}"),
    }
}

/// Set a radio/menu config to the first choice whose (lowercased) label
/// contains one of `wanted`, in priority order. Returns whether it stuck.
fn set_choice_matching(camera: &Camera, key: &str, wanted: &[&str]) -> bool {
    let Ok(widget) = camera.config_key::<RadioWidget>(key).wait() else {
        return false;
    };
    let choices: Vec<String> = widget.choices_iter().collect();
    let pick = wanted.iter().find_map(|w| {
        choices
            .iter()
            .find(|c| c.to_lowercase().contains(w))
            .cloned()
    });
    apply_choice(camera, &widget, key, pick)
}

/// Set an image-size config to the smallest resolution available, parsing
/// `WxH` labels where possible and otherwise falling back to a "small" label.
fn set_smallest_size(camera: &Camera, key: &str) -> bool {
    let Ok(widget) = camera.config_key::<RadioWidget>(key).wait() else {
        return false;
    };
    let choices: Vec<String> = widget.choices_iter().collect();
    let by_area = choices
        .iter()
        .filter(|c| parse_area(c).is_some())
        .min_by_key(|c| parse_area(c).unwrap())
        .cloned();
    let pick = by_area.or_else(|| {
        choices
            .iter()
            .find(|c| c.to_lowercase().contains("small"))
            .cloned()
    });
    apply_choice(camera, &widget, key, pick)
}

/// Push a chosen value to the camera and log the outcome.
fn apply_choice(camera: &Camera, widget: &RadioWidget, key: &str, pick: Option<String>) -> bool {
    let Some(choice) = pick else { return false };
    if widget.set_choice(&choice).is_ok() && camera.set_config(widget).wait().is_ok() {
        eprintln!("[camera] {key} = {choice}");
        true
    } else {
        false
    }
}

/// Parse a `"6000x4000"`-style label into a pixel area.
fn parse_area(label: &str) -> Option<u64> {
    let (w, h) = label.split_once(['x', 'X'])?;
    let w: u64 = w.trim().parse().ok()?;
    let h: u64 = h.trim().parse().ok()?;
    Some(w * h)
}

/// Grab a single live-view frame and decode it to an egui image.
fn grab_preview(camera: &Camera, context: &Context) -> Result<ColorImage, String> {
    let file = camera
        .capture_preview()
        .wait()
        .map_err(|e| format!("Live preview failed: {e}"))?;
    let data = file
        .get_data(context)
        .wait()
        .map_err(|e| format!("Reading preview failed: {e}"))?;
    let img = image::load_from_memory(&data)
        .map_err(|e| format!("Decoding preview failed: {e}"))?
        .to_rgba8();
    Ok(to_color_image(&img))
}

/// Trigger the shutter, download the resulting file, and decode it.
/// A single full capture: decoded for display/compositing, plus the original
/// camera file bytes (and extension) so the raw shot can be saved verbatim.
struct Capture {
    image: RgbaImage,
    bytes: Box<[u8]>,
    ext: String,
}

fn capture_full(camera: &Camera, context: &Context) -> Result<Capture, String> {
    let path = camera
        .capture_image()
        .wait()
        .map_err(|e| format!("Capture failed: {e}"))?;
    let folder = path.folder().to_string();
    let name = path.name().to_string();
    let file = camera
        .fs()
        .download(&folder, &name)
        .wait()
        .map_err(|e| format!("Downloading photo failed: {e}"))?;
    let data = file
        .get_data(context)
        .wait()
        .map_err(|e| format!("Reading photo failed: {e}"))?;
    let image = image::load_from_memory(&data)
        .map_err(|e| format!("Decoding photo failed: {e}"))?
        .to_rgba8();
    let ext = std::path::Path::new(&name)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_else(|| "jpg".to_string());
    Ok(Capture {
        image,
        bytes: data,
        ext,
    })
}

/// Convert an `image` RGBA buffer into an egui `ColorImage`.
fn to_color_image(img: &RgbaImage) -> ColorImage {
    ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw())
}

/// Shrink an image so neither side exceeds `max_side`, preserving aspect ratio.
fn downscale(img: &RgbaImage, max_side: u32) -> RgbaImage {
    let (w, h) = (img.width(), img.height());
    if w <= max_side && h <= max_side {
        return img.clone();
    }
    let scale = f64::from(max_side) / f64::from(w.max(h));
    let nw = (f64::from(w) * scale).round().max(1.0) as u32;
    let nh = (f64::from(h) * scale).round().max(1.0) as u32;
    imageops::resize(img, nw, nh, imageops::FilterType::Triangle)
}

/// Report a fatal startup error and ask the UI to repaint.
fn fail(ctx: &EguiContext, tx: &Sender<Event>, msg: String) {
    let _ = tx.send(Event::Error(msg));
    ctx.request_repaint();
}
