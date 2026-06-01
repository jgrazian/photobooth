//! egui front-end: renders the photobooth UI and translates camera-thread
//! events into on-screen state. All blocking camera work lives in `camera.rs`.

use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use eframe::CreationContext;
use egui::{
    Align2, Color32, Context as EguiContext, FontId, Pos2, Rect, Stroke, TextureHandle,
    TextureOptions, Vec2,
};

use crate::camera::{self, Cmd, Event, Status, SHOTS};
use crate::email;

/// What the UI is currently showing. Mirrors [`camera::Status`] but adds the
/// connection lifecycle states the UI owns.
enum Phase {
    Connecting,
    Ready,
    Countdown { shot: usize, remaining: u32 },
    Capturing { shot: usize },
    Review { shot: usize },
    Compositing,
    Finished,
    Error,
}

/// State of the email-entry overlay (shown on top of the finished screen).
enum SendState {
    /// Typing an address on the on-screen keyboard.
    Editing,
    /// Email is being sent in the background.
    Sending,
    /// Send failed; the message is shown and the keyboard stays up for a retry.
    Failed(String),
}

pub struct PhotoboothApp {
    egui_ctx: EguiContext,
    cmd_tx: Sender<Cmd>,
    evt_rx: Receiver<Event>,
    worker: Option<JoinHandle<()>>,

    phase: Phase,
    model: Option<String>,
    error: Option<String>,
    saved_path: Option<String>,
    /// Transient caption shown in the control bar (e.g. after a successful send).
    toast: Option<String>,

    /// `Some` while the email-entry overlay is up.
    send_state: Option<SendState>,
    /// Address being typed on the on-screen keyboard.
    email_input: String,
    /// Result channel for an in-flight background send.
    pending_send: Option<Receiver<Result<(), String>>>,

    live: Option<TextureHandle>,
    last_captured: Option<TextureHandle>,
    composite: Option<TextureHandle>,
    thumbs: Vec<Option<TextureHandle>>,
}

impl PhotoboothApp {
    pub fn new(cc: &CreationContext<'_>) -> Self {
        let egui_ctx = cc.egui_ctx.clone();
        egui_ctx.set_visuals(egui::Visuals::dark());
        let worker = camera::spawn(egui_ctx.clone());
        Self {
            egui_ctx,
            cmd_tx: worker.cmd_tx,
            evt_rx: worker.evt_rx,
            worker: Some(worker.join),
            phase: Phase::Connecting,
            model: None,
            error: None,
            saved_path: None,
            toast: None,
            send_state: None,
            email_input: String::new(),
            pending_send: None,
            live: None,
            last_captured: None,
            composite: None,
            thumbs: vec![None; SHOTS],
        }
    }

    /// Tear down the current worker and start a fresh one (used by "Retry").
    /// Joining the old worker first guarantees its camera is released before we
    /// try to claim the device again.
    fn reconnect(&mut self) {
        self.shutdown_worker();
        let worker = camera::spawn(self.egui_ctx.clone());
        self.cmd_tx = worker.cmd_tx;
        self.evt_rx = worker.evt_rx;
        self.worker = Some(worker.join);
        self.phase = Phase::Connecting;
        self.error = None;
        self.live = None;
    }

    /// Ask the worker to quit and wait for it, so the camera (released when the
    /// worker's `Camera` drops) is actually freed before we move on.
    fn shutdown_worker(&mut self) {
        let _ = self.cmd_tx.send(Cmd::Quit);
        if let Some(join) = self.worker.take() {
            let _ = join.join();
        }
    }

    /// Kick off a new session, clearing any previous results.
    fn begin_session(&mut self) {
        self.composite = None;
        self.last_captured = None;
        self.saved_path = None;
        self.toast = None;
        self.thumbs = vec![None; SHOTS];
        self.phase = Phase::Countdown { shot: 0, remaining: 3 };
        let _ = self.cmd_tx.send(Cmd::Start);
    }

    /// Open the on-screen keyboard to collect an email address.
    fn open_email_entry(&mut self) {
        self.email_input.clear();
        self.send_state = Some(SendState::Editing);
    }

    /// Close the email overlay without sending.
    fn cancel_email_entry(&mut self) {
        self.send_state = None;
        self.email_input.clear();
    }

    /// Kick off the background email send for the current composite.
    fn start_send(&mut self) {
        let Some(path) = self.saved_path.clone() else {
            self.send_state = Some(SendState::Failed("No saved photo to send".into()));
            return;
        };
        let to = self.email_input.trim().to_string();
        let ctx = self.egui_ctx.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = email::send_photo(&to, Path::new(&path));
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        self.pending_send = Some(rx);
        self.send_state = Some(SendState::Sending);
    }

    /// Check for a completed background send and react to the result.
    fn poll_send(&mut self) {
        let Some(rx) = &self.pending_send else { return };
        let Ok(result) = rx.try_recv() else { return };
        self.pending_send = None;
        match result {
            Ok(()) => {
                // Reset back to the live "Take Photo" screen for the next guest.
                self.send_state = None;
                self.email_input.clear();
                self.composite = None;
                self.last_captured = None;
                self.saved_path = None;
                self.thumbs = vec![None; SHOTS];
                self.toast = Some("Photo sent ✔".to_string());
                self.phase = Phase::Ready;
                let _ = self.cmd_tx.send(Cmd::Preview);
            }
            Err(e) => self.send_state = Some(SendState::Failed(e)),
        }
    }

    /// Drain everything the camera thread has sent us since the last frame.
    fn pump_events(&mut self) {
        while let Ok(ev) = self.evt_rx.try_recv() {
            match ev {
                Event::Connected(model) => {
                    self.model = Some(model);
                    if matches!(self.phase, Phase::Connecting) {
                        self.phase = Phase::Ready;
                    }
                }
                Event::Preview(image) => {
                    upload(&self.egui_ctx, &mut self.live, "live", image);
                }
                Event::Captured { index, image } => {
                    upload(&self.egui_ctx, &mut self.last_captured, "captured", image.clone());
                    if let Some(slot) = self.thumbs.get_mut(index) {
                        *slot = Some(self.egui_ctx.load_texture(
                            format!("thumb-{index}"),
                            image,
                            TextureOptions::LINEAR,
                        ));
                    }
                }
                Event::Composite { image, saved } => {
                    upload(&self.egui_ctx, &mut self.composite, "composite", image);
                    self.saved_path = saved;
                }
                Event::Status(status) => self.phase = phase_from(status),
                Event::Error(msg) => {
                    self.error = Some(msg);
                    self.phase = Phase::Error;
                }
            }
        }
    }
}

/// Height of the bottom control bar.
const BAR_H: f32 = 132.0;
/// "Take Photo" button colour.
const GREEN: Color32 = Color32::from_rgb(0, 150, 90);
/// "Send Photo" button colour.
const BLUE: Color32 = Color32::from_rgb(0, 120, 210);

impl eframe::App for PhotoboothApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.pump_events();
        self.poll_send();

        // Keep spinners/animations alive while we're waiting on the camera or a send.
        if self.pending_send.is_some()
            || matches!(self.phase, Phase::Connecting | Phase::Compositing | Phase::Capturing { .. })
        {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }

        let full = ui.max_rect();

        // The email-entry overlay takes over the whole window when active.
        if self.send_state.is_some() {
            ui.painter().rect_filled(full, 0.0, Color32::from_rgb(16, 16, 20));
            self.draw_email_screen(ui, full);
            return;
        }

        // Otherwise: split the window into an image area (top) and control bar (bottom).
        let split_y = (full.max.y - BAR_H).max(full.min.y);
        let image_rect = Rect::from_min_max(full.min, Pos2::new(full.max.x, split_y));
        let bar_rect = Rect::from_min_max(Pos2::new(full.min.x, split_y), full.max);

        // The Ui we're handed has no background; paint our own.
        ui.painter().rect_filled(image_rect, 0.0, Color32::from_rgb(16, 16, 20));
        ui.painter().rect_filled(bar_rect, 0.0, Color32::from_rgb(26, 26, 32));
        ui.painter().hline(
            full.x_range(),
            bar_rect.min.y,
            Stroke::new(1.0, Color32::from_white_alpha(20)),
        );

        self.draw_image_area(ui, image_rect);
        self.draw_controls(ui, bar_rect);
    }

    fn on_exit(&mut self) {
        self.shutdown_worker();
    }
}

impl PhotoboothApp {
    /// Render the live preview / captured photo / composite plus per-phase
    /// overlays into the image area (never the control bar).
    fn draw_image_area(&mut self, ui: &mut egui::Ui, rect: Rect) {
        // Background image for the current phase.
        match &self.phase {
            Phase::Ready | Phase::Countdown { .. } | Phase::Capturing { .. } => {
                paint_fit(ui, rect, self.live.as_ref());
            }
            Phase::Review { .. } => paint_fit(ui, rect, self.last_captured.as_ref()),
            Phase::Finished => paint_fit(ui, rect, self.composite.as_ref()),
            _ => {}
        }

        // Overlays.
        match self.phase {
            Phase::Connecting => center_message(ui, rect, "Connecting to camera…", Color32::WHITE),
            Phase::Error => self.draw_error_message(ui, rect),
            Phase::Ready => {
                if let Some(model) = &self.model {
                    ui.painter().text(
                        rect.center_top() + Vec2::new(0.0, 24.0),
                        Align2::CENTER_TOP,
                        model,
                        FontId::proportional(18.0),
                        Color32::from_gray(170),
                    );
                }
            }
            Phase::Countdown { shot, remaining } => draw_countdown(ui, rect, shot, remaining),
            Phase::Capturing { shot } => {
                progress_badge(ui, rect, shot);
                center_message(ui, rect, "Smile!", Color32::WHITE);
            }
            Phase::Review { shot } => {
                progress_badge(ui, rect, shot);
                corner_check(ui, rect);
            }
            Phase::Compositing => center_message(ui, rect, "Creating your photo…", Color32::WHITE),
            Phase::Finished => {}
        }

        // Thumbnail strip of shots taken so far (during an active session).
        if matches!(
            self.phase,
            Phase::Countdown { .. } | Phase::Capturing { .. } | Phase::Review { .. }
        ) {
            self.draw_thumb_strip(ui, rect);
        }
    }

    /// Render the bottom control bar: the green "Take Photo" button (plus the
    /// blue "Send to Text" button once a composite exists), or a Retry button
    /// while in the error state.
    fn draw_controls(&mut self, ui: &mut egui::Ui, bar: Rect) {
        let btn_h = 76.0;
        let y = bar.center().y - btn_h / 2.0;

        // Error state: a single Retry button.
        if matches!(self.phase, Phase::Error) {
            let rect = Rect::from_center_size(
                Pos2::new(bar.center().x, bar.center().y),
                Vec2::new(240.0, btn_h),
            );
            if place_button(ui, rect, "Retry", Color32::from_rgb(120, 120, 130), true) {
                self.reconnect();
            }
            return;
        }

        // "Take Photo" is only actionable once the camera is idle and ready
        // (or after a finished set); it's greyed out while connecting or mid-run.
        let finished = matches!(self.phase, Phase::Finished);
        let can_take = matches!(self.phase, Phase::Ready) || finished;
        let show_send = finished && self.composite.is_some();

        let take_w = 300.0;
        let send_w = 300.0;
        let gap = 28.0;
        let total_w = if show_send { take_w + gap + send_w } else { take_w };
        let mut x = bar.center().x - total_w / 2.0;

        let take_label = if finished { "Take New Photo" } else { "Take Photo" };
        let take_rect = Rect::from_min_size(Pos2::new(x, y), Vec2::new(take_w, btn_h));
        if place_button(ui, take_rect, take_label, GREEN, can_take) {
            self.begin_session();
        }
        x += take_w + gap;

        if show_send {
            let send_rect = Rect::from_min_size(Pos2::new(x, y), Vec2::new(send_w, btn_h));
            if place_button(ui, send_rect, "Send Photo", BLUE, true) {
                self.open_email_entry();
            }
        }

        // Caption above the buttons: a transient toast, else the saved path.
        let caption = self.toast.clone().or_else(|| {
            finished
                .then(|| self.saved_path.clone())
                .flatten()
                .map(|p| format!("Saved to {p}"))
        });
        if let Some(text) = caption {
            ui.painter().text(
                Pos2::new(bar.center().x, bar.min.y + 12.0),
                Align2::CENTER_TOP,
                text,
                FontId::proportional(15.0),
                Color32::from_gray(190),
            );
        }
    }

    /// Centred error text drawn in the image area; the Retry button lives in the
    /// control bar.
    fn draw_error_message(&self, ui: &egui::Ui, rect: Rect) {
        let painter = ui.painter();
        painter.text(
            rect.center() - Vec2::new(0.0, 26.0),
            Align2::CENTER_CENTER,
            "⚠ Camera problem",
            FontId::proportional(30.0),
            Color32::from_rgb(240, 120, 120),
        );
        let msg = self.error.as_deref().unwrap_or("Unknown error");
        painter.text(
            rect.center() + Vec2::new(0.0, 18.0),
            Align2::CENTER_CENTER,
            msg,
            FontId::proportional(18.0),
            Color32::from_gray(210),
        );
    }

    /// Full-screen email-entry overlay: address display, on-screen QWERTY
    /// keyboard, and Send / Cancel. Shown on top of the finished composite.
    fn draw_email_screen(&mut self, ui: &mut egui::Ui, full: Rect) {
        let cx = full.center().x;

        ui.painter().text(
            Pos2::new(cx, full.min.y + 34.0),
            Align2::CENTER_TOP,
            "Enter your email to get your photo",
            FontId::proportional(34.0),
            Color32::WHITE,
        );

        // Typed-address display box.
        let box_rect = Rect::from_center_size(Pos2::new(cx, full.min.y + 122.0), Vec2::new(820.0, 70.0));
        ui.painter().rect_filled(box_rect, 10.0, Color32::from_rgb(32, 32, 40));
        ui.painter().rect_stroke(
            box_rect,
            10.0,
            Stroke::new(2.0, Color32::from_white_alpha(50)),
            egui::StrokeKind::Inside,
        );
        let (shown, color) = if self.email_input.is_empty() {
            ("you@example.com".to_string(), Color32::from_gray(110))
        } else {
            (self.email_input.clone(), Color32::WHITE)
        };
        ui.painter().text(
            box_rect.left_center() + Vec2::new(22.0, 0.0),
            Align2::LEFT_CENTER,
            shown,
            FontId::monospace(30.0),
            color,
        );

        // While sending, replace the keyboard with a status line.
        if matches!(self.send_state, Some(SendState::Sending)) {
            ui.painter().text(
                full.center(),
                Align2::CENTER_CENTER,
                "Sending…",
                FontId::proportional(42.0),
                Color32::WHITE,
            );
            return;
        }

        if let Some(SendState::Failed(msg)) = &self.send_state {
            ui.painter().text(
                Pos2::new(cx, full.min.y + 178.0),
                Align2::CENTER_TOP,
                format!("Couldn't send: {msg}"),
                FontId::proportional(18.0),
                Color32::from_rgb(240, 130, 130),
            );
        }

        // On-screen keyboard.
        const ROWS: [&[&str]; 5] = [
            &["1", "2", "3", "4", "5", "6", "7", "8", "9", "0"],
            &["q", "w", "e", "r", "t", "y", "u", "i", "o", "p"],
            &["a", "s", "d", "f", "g", "h", "j", "k", "l"],
            &["z", "x", "c", "v", "b", "n", "m"],
            &["@", ".", "_", "-", ".com", "Del"],
        ];
        let kw = 100.0;
        let kh = 78.0;
        let gap = 10.0;
        let y0 = full.min.y + 214.0;
        let mut typed: Option<&str> = None;
        for (i, row) in ROWS.iter().enumerate() {
            let y = y0 + i as f32 * (kh + gap);
            let total = row.len() as f32 * kw + (row.len() as f32 - 1.0) * gap;
            let mut x = cx - total / 2.0;
            for &key in row.iter() {
                let r = Rect::from_min_size(Pos2::new(x, y), Vec2::new(kw, kh));
                if place_key(ui, r, key) {
                    typed = Some(key);
                }
                x += kw + gap;
            }
        }
        if let Some(key) = typed {
            match key {
                "Del" => {
                    self.email_input.pop();
                }
                other => self.email_input.push_str(other),
            }
        }

        // Send / Cancel.
        let btn_h = 80.0;
        let by = full.max.y - btn_h - 28.0;
        let bw = 300.0;
        let bgap = 40.0;
        let mut bx = cx - (bw * 2.0 + bgap) / 2.0;
        let cancel_rect = Rect::from_min_size(Pos2::new(bx, by), Vec2::new(bw, btn_h));
        if place_button(ui, cancel_rect, "Cancel", Color32::from_rgb(90, 90, 100), true) {
            self.cancel_email_entry();
        }
        bx += bw + bgap;
        let send_rect = Rect::from_min_size(Pos2::new(bx, by), Vec2::new(bw, btn_h));
        if place_button(ui, send_rect, "Send", GREEN, valid_email(&self.email_input)) {
            self.start_send();
        }
    }

    fn draw_thumb_strip(&self, ui: &mut egui::Ui, rect: Rect) {
        let size = 90.0;
        let gap = 12.0;
        let total = SHOTS as f32 * size + (SHOTS as f32 - 1.0) * gap;
        let mut x = rect.center().x - total / 2.0;
        let y = rect.bottom() - size - 20.0;
        for slot in &self.thumbs {
            let cell = Rect::from_min_size(Pos2::new(x, y), Vec2::splat(size));
            ui.painter().rect_filled(cell, 8.0, Color32::from_black_alpha(120));
            if let Some(tex) = slot {
                paint_fit(ui, cell.shrink(3.0), Some(tex));
            }
            ui.painter().rect_stroke(
                cell,
                8.0,
                Stroke::new(2.0, Color32::from_white_alpha(60)),
                egui::StrokeKind::Inside,
            );
            x += size + gap;
        }
    }
}

/// Place a filled, rounded button filling `rect`. When `enabled` is false it is
/// greyed out and unclickable. Returns whether it was clicked this frame.
fn place_button(ui: &mut egui::Ui, rect: Rect, label: &str, fill: Color32, enabled: bool) -> bool {
    let button = egui::Button::new(egui::RichText::new(label).size(28.0).color(Color32::WHITE))
        .fill(fill)
        .corner_radius(14.0)
        .min_size(rect.size());
    ui.add_enabled_ui(enabled, |ui| ui.put(rect, button).clicked())
        .inner
}

/// Place a single keyboard key filling `rect`. Returns whether it was tapped.
fn place_key(ui: &mut egui::Ui, rect: Rect, label: &str) -> bool {
    let size = if label.chars().count() > 1 { 24.0 } else { 30.0 };
    let key = egui::Button::new(egui::RichText::new(label).size(size).color(Color32::WHITE))
        .fill(Color32::from_rgb(48, 48, 58))
        .corner_radius(10.0)
        .min_size(rect.size());
    ui.put(rect, key).clicked()
}

/// Minimal email sanity check: `local@domain` with a dot in the domain.
fn valid_email(s: &str) -> bool {
    let s = s.trim();
    match s.split_once('@') {
        Some((local, domain)) => {
            !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.')
        }
        None => false,
    }
}

/// Upload (or refresh) a texture in `slot`.
fn upload(ctx: &EguiContext, slot: &mut Option<TextureHandle>, name: &str, image: egui::ColorImage) {
    match slot {
        Some(handle) => handle.set(image, TextureOptions::LINEAR),
        None => *slot = Some(ctx.load_texture(name, image, TextureOptions::LINEAR)),
    }
}

/// Draw a texture centred inside `rect`, preserving aspect ratio (letterboxed).
fn paint_fit(ui: &egui::Ui, rect: Rect, tex: Option<&TextureHandle>) {
    let Some(tex) = tex else { return };
    let size = tex.size_vec2();
    if size.x <= 0.0 || size.y <= 0.0 {
        return;
    }
    let scale = (rect.width() / size.x).min(rect.height() / size.y);
    let draw = size * scale;
    let draw_rect = Rect::from_center_size(rect.center(), draw);
    let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
    ui.painter().image(tex.id(), draw_rect, uv, Color32::WHITE);
}

/// Big translucent countdown number over the live preview.
fn draw_countdown(ui: &egui::Ui, rect: Rect, shot: usize, remaining: u32) {
    let painter = ui.painter();
    let center = rect.center();
    painter.circle_filled(center, 130.0, Color32::from_black_alpha(140));
    painter.text(
        center,
        Align2::CENTER_CENTER,
        remaining.to_string(),
        FontId::proportional(190.0),
        Color32::WHITE,
    );
    progress_badge(ui, rect, shot);
}

/// "Photo N of 4" badge at the top of the screen.
fn progress_badge(ui: &egui::Ui, rect: Rect, shot: usize) {
    let painter = ui.painter();
    let pos = rect.center_top() + Vec2::new(0.0, 30.0);
    let text = format!("Photo {} of {}", shot + 1, SHOTS);
    let galley = painter.layout_no_wrap(text, FontId::proportional(26.0), Color32::WHITE);
    let pad = Vec2::new(18.0, 8.0);
    let bg = Rect::from_center_size(pos, galley.size() + pad * 2.0);
    painter.rect_filled(bg, 10.0, Color32::from_black_alpha(140));
    painter.galley(bg.center() - galley.size() / 2.0, galley, Color32::WHITE);
}

/// A green check badge shown briefly after each shot.
fn corner_check(ui: &egui::Ui, rect: Rect) {
    let painter = ui.painter();
    let center = rect.center();
    painter.circle_filled(center, 80.0, Color32::from_rgba_unmultiplied(0, 150, 90, 200));
    painter.text(
        center,
        Align2::CENTER_CENTER,
        "✔",
        FontId::proportional(96.0),
        Color32::WHITE,
    );
}

/// Centre a single line of large text.
fn center_message(ui: &egui::Ui, rect: Rect, text: &str, color: Color32) {
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        text,
        FontId::proportional(54.0),
        color,
    );
}

/// Map a camera-thread [`Status`] onto a UI [`Phase`].
fn phase_from(status: Status) -> Phase {
    match status {
        Status::Idle => Phase::Ready,
        Status::Countdown { shot, remaining } => Phase::Countdown { shot, remaining },
        Status::Capturing { shot } => Phase::Capturing { shot },
        Status::Review { shot } => Phase::Review { shot },
        Status::Compositing => Phase::Compositing,
        Status::Done => Phase::Finished,
    }
}
