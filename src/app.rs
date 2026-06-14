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

use crate::camera::{self, Cmd, Event, SessionConfig, Status};
use crate::email;
use crate::outbox;

/// How the finished photo is delivered to the guest.
///
/// Default is [`SendMode::Sms`] (drop into the outbox for the Mac iMessage
/// watcher); the original email path is kept available behind the
/// `PHOTOBOOTH_SEND_MODE=email` flag.
#[derive(Clone, Copy, PartialEq)]
enum SendMode {
    /// Collect a phone number; queue the photo into the outbox folder.
    Sms,
    /// Collect an email address; send the photo over SMTP.
    Email,
}

impl SendMode {
    fn from_env() -> Self {
        match std::env::var("PHOTOBOOTH_SEND_MODE").ok().as_deref() {
            Some(v) if v.eq_ignore_ascii_case("email") => SendMode::Email,
            _ => SendMode::Sms,
        }
    }
}

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

/// State of the recipient-entry overlay (shown on top of the finished screen).
enum SendState {
    /// Typing a phone number / address on the on-screen keyboard.
    Editing,
    /// The photo is being delivered in the background (queued or emailed).
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

    /// Guest-chosen capture settings (shot count + countdown), edited via the
    /// round config button and applied at the next [`Self::begin_session`].
    config: SessionConfig,
    /// `true` while the full-screen settings picker is up.
    show_config: bool,

    /// How the finished photo is delivered (phone/iMessage vs. email).
    send_mode: SendMode,
    /// `Some` while the recipient-entry overlay is up.
    send_state: Option<SendState>,
    /// Phone number or email address being typed on the on-screen keyboard.
    recipient_input: String,
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
        let config = SessionConfig::default();
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
            config,
            show_config: false,
            send_mode: SendMode::from_env(),
            send_state: None,
            recipient_input: String::new(),
            pending_send: None,
            live: None,
            last_captured: None,
            composite: None,
            thumbs: vec![None; config.shots],
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
        self.thumbs = vec![None; self.config.shots];
        self.phase = Phase::Countdown { shot: 0, remaining: self.config.countdown_secs as u32 };
        let _ = self.cmd_tx.send(Cmd::Start(self.config));
    }

    /// Open the on-screen keyboard to collect a phone number (or email).
    fn open_send_entry(&mut self) {
        self.recipient_input.clear();
        self.send_state = Some(SendState::Editing);
    }

    /// Close the recipient overlay without sending.
    fn cancel_send_entry(&mut self) {
        self.send_state = None;
        self.recipient_input.clear();
    }

    /// Kick off the background delivery of the current composite: queue it into
    /// the outbox (SMS mode) or send it over SMTP (email mode).
    fn start_send(&mut self) {
        let Some(path) = self.saved_path.clone() else {
            self.send_state = Some(SendState::Failed("No saved photo to send".into()));
            return;
        };
        let to = self.recipient_input.trim().to_string();
        let mode = self.send_mode;
        let ctx = self.egui_ctx.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = match mode {
                SendMode::Sms => outbox::queue(&to, Path::new(&path)),
                SendMode::Email => email::send_photo(&to, Path::new(&path)),
            };
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
                self.recipient_input.clear();
                self.composite = None;
                self.last_captured = None;
                self.saved_path = None;
                self.thumbs = vec![None; self.config.shots];
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
        // F11 toggles fullscreen.
        if ui.input(|i| i.key_pressed(egui::Key::F11)) {
            let fullscreen = ui.ctx().input(|i| i.viewport().fullscreen.unwrap_or(false));
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fullscreen));
        }

        self.pump_events();
        self.poll_send();

        // Keep spinners/animations alive while we're waiting on the camera or a send.
        if self.pending_send.is_some()
            || matches!(self.phase, Phase::Connecting | Phase::Compositing | Phase::Capturing { .. })
        {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }

        let full = ui.max_rect();

        // The recipient-entry overlay takes over the whole window when active.
        if self.send_state.is_some() {
            ui.painter().rect_filled(full, 0.0, Color32::from_rgb(16, 16, 20));
            self.draw_send_screen(ui, full);
            return;
        }

        // The settings picker likewise takes over the whole window.
        if self.show_config {
            ui.painter().rect_filled(full, 0.0, Color32::from_rgb(16, 16, 20));
            self.draw_config_screen(ui, full);
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

        // Round settings button, overlaid on the top-right of the live preview /
        // finished photo. Only offered while idle, so the active shot count and
        // timer can't change mid-session.
        if matches!(self.phase, Phase::Ready | Phase::Finished) {
            self.draw_config_button(ui, image_rect);
        }
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
            Phase::Countdown { shot, remaining } => {
                draw_countdown(ui, rect, shot, remaining, self.config.shots)
            }
            Phase::Capturing { shot } => {
                progress_badge(ui, rect, shot, self.config.shots);
                center_message(ui, rect, "Smile!", Color32::WHITE);
            }
            Phase::Review { shot } => {
                progress_badge(ui, rect, shot, self.config.shots);
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
    /// blue "Send Photo" button once a composite exists), or a Retry button
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
                self.open_send_entry();
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

    /// Full-screen recipient-entry overlay: the typed phone number / email,
    /// an on-screen keypad (numeric in SMS mode, QWERTY in email mode), and
    /// Send / Cancel. Shown on top of the finished composite.
    fn draw_send_screen(&mut self, ui: &mut egui::Ui, full: Rect) {
        let sms = self.send_mode == SendMode::Sms;
        let cx = full.center().x;
        let w = full.width();
        let h = full.height();

        // Title.
        let title = if sms {
            "Enter your phone number to get your photo"
        } else {
            "Enter your email to get your photo"
        };
        let title_size = (h * 0.045).clamp(18.0, 34.0);
        let title_y = full.min.y + h * 0.025;
        ui.painter().text(
            Pos2::new(cx, title_y),
            Align2::CENTER_TOP,
            title,
            FontId::proportional(title_size),
            Color32::WHITE,
        );

        // Typed value display box — deliberately compact (narrower and shorter
        // than the keyboard) so the keypad has room on short screens.
        let box_w = (w * 0.55).clamp(340.0, 560.0);
        let box_h = (h * 0.085).clamp(38.0, 60.0);
        let box_cy = title_y + title_size + box_h * 0.5 + 6.0;
        let box_rect = Rect::from_center_size(Pos2::new(cx, box_cy), Vec2::new(box_w, box_h));
        ui.painter().rect_filled(box_rect, 10.0, Color32::from_rgb(32, 32, 40));
        ui.painter().rect_stroke(
            box_rect,
            10.0,
            Stroke::new(2.0, Color32::from_white_alpha(50)),
            egui::StrokeKind::Inside,
        );
        let placeholder = if sms { "+1 555 123 4567" } else { "you@example.com" };
        let (shown, color) = if self.recipient_input.is_empty() {
            (placeholder.to_string(), Color32::from_gray(110))
        } else {
            (self.recipient_input.clone(), Color32::WHITE)
        };
        ui.painter().text(
            box_rect.left_center() + Vec2::new(16.0, 0.0),
            Align2::LEFT_CENTER,
            shown,
            FontId::monospace((box_h * 0.5).clamp(18.0, 28.0)),
            color,
        );

        // While sending, replace the keyboard with a status line.
        if matches!(self.send_state, Some(SendState::Sending)) {
            ui.painter().text(
                full.center(),
                Align2::CENTER_CENTER,
                "Sending…",
                FontId::proportional((h * 0.06).clamp(26.0, 42.0)),
                Color32::WHITE,
            );
            return;
        }

        // Reserve the bottom band for Send / Cancel before laying out the keys.
        let btn_h = (h * 0.11).clamp(44.0, 80.0);
        let btn_y = full.max.y - btn_h - h * 0.025;
        let btn_w = (w * 0.3).clamp(170.0, 300.0);
        let btn_gap = (w * 0.04).clamp(16.0, 40.0);

        // Failure message sits just under the display box.
        if let Some(SendState::Failed(msg)) = &self.send_state {
            ui.painter().text(
                Pos2::new(cx, box_rect.bottom() + 4.0),
                Align2::CENTER_TOP,
                format!("Couldn't send: {msg}"),
                FontId::proportional((h * 0.028).clamp(12.0, 18.0)),
                Color32::from_rgb(240, 130, 130),
            );
        }

        // On-screen keyboard: a phone keypad in SMS mode, a QWERTY layout for
        // email. Keys are sized to fit the band between the box and the buttons
        // (height) and the window width — so the whole picker fits any screen.
        const PHONE_ROWS: [&[&str]; 4] = [
            &["1", "2", "3"],
            &["4", "5", "6"],
            &["7", "8", "9"],
            &["+", "0", "Del"],
        ];
        const EMAIL_ROWS: [&[&str]; 5] = [
            &["1", "2", "3", "4", "5", "6", "7", "8", "9", "0"],
            &["q", "w", "e", "r", "t", "y", "u", "i", "o", "p"],
            &["a", "s", "d", "f", "g", "h", "j", "k", "l"],
            &["z", "x", "c", "v", "b", "n", "m"],
            &["@", ".", "_", "-", ".com", "Del"],
        ];
        let rows: &[&[&str]] = if sms { &PHONE_ROWS } else { &EMAIL_ROWS };
        let nrows = rows.len() as f32;
        let max_cols = rows.iter().map(|r| r.len()).max().unwrap_or(1) as f32;

        let gap = (w * 0.008).clamp(6.0, 16.0);
        let kb_top = box_rect.bottom() + (h * 0.04).max(18.0);
        let kb_bottom = btn_y - h * 0.02;
        let band_h = (kb_bottom - kb_top).max(0.0);
        let band_w = w * 0.96;

        // Fit keys within both the band height and the available width; cap the
        // width so the few keypad keys don't balloon on a wide screen.
        let kh = ((band_h - gap * (nrows - 1.0)) / nrows).clamp(26.0, 88.0);
        let kw_fit = (band_w - gap * (max_cols - 1.0)) / max_cols;
        let kw = if sms { kw_fit.min(150.0) } else { kw_fit.min(110.0) };

        // Vertically centre the key block within its band.
        let block_h = kh * nrows + gap * (nrows - 1.0);
        let y0 = kb_top + (band_h - block_h).max(0.0) * 0.5;

        let mut typed: Option<&str> = None;
        for (i, row) in rows.iter().enumerate() {
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
                    self.recipient_input.pop();
                }
                other => self.recipient_input.push_str(other),
            }
        }

        // Send / Cancel.
        let mut bx = cx - (btn_w * 2.0 + btn_gap) / 2.0;
        let cancel_rect = Rect::from_min_size(Pos2::new(bx, btn_y), Vec2::new(btn_w, btn_h));
        if place_button(ui, cancel_rect, "Cancel", Color32::from_rgb(90, 90, 100), true) {
            self.cancel_send_entry();
        }
        bx += btn_w + btn_gap;
        let send_rect = Rect::from_min_size(Pos2::new(bx, btn_y), Vec2::new(btn_w, btn_h));
        let valid = if sms {
            valid_phone(&self.recipient_input)
        } else {
            valid_email(&self.recipient_input)
        };
        if place_button(ui, send_rect, "Send", GREEN, valid) {
            self.start_send();
        }
    }

    fn draw_thumb_strip(&self, ui: &mut egui::Ui, rect: Rect) {
        let size = 90.0;
        let gap = 12.0;
        let n = self.thumbs.len().max(1) as f32;
        let total = n * size + (n - 1.0) * gap;
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

    /// Round settings button in the top-right of the image area. Its face is a
    /// black-on-white icon of the currently chosen shot layout; tapping it opens
    /// the settings picker.
    fn draw_config_button(&mut self, ui: &mut egui::Ui, image_rect: Rect) {
        let r = 34.0;
        let margin = 24.0;
        let center = Pos2::new(
            image_rect.max.x - margin - r,
            image_rect.min.y + margin + r,
        );
        let hit = Rect::from_center_size(center, Vec2::splat(r * 2.0));
        let resp = ui.interact(hit, ui.id().with("config-btn"), egui::Sense::click());

        // Darken behind the icon so it reads over a bright preview, and lift it
        // slightly on hover.
        ui.painter()
            .circle_filled(center, r + 5.0, Color32::from_black_alpha(120));
        let bg = if resp.hovered() {
            Color32::WHITE
        } else {
            Color32::from_gray(235)
        };
        count_icon(ui.painter(), center, r, self.config.shots, Color32::BLACK, bg);

        if resp.clicked() {
            self.show_config = true;
        }
    }

    /// Full-screen settings picker: choose the shot count (1 / 2 / 4) and the
    /// countdown timer (3 / 5 / 7 seconds), then Done. Changes apply to the next
    /// session.
    fn draw_config_screen(&mut self, ui: &mut egui::Ui, full: Rect) {
        let cx = full.center().x;
        let (w, h) = (full.width(), full.height());

        // Title.
        let title_size = (h * 0.05).clamp(22.0, 40.0);
        ui.painter().text(
            Pos2::new(cx, full.min.y + h * 0.06),
            Align2::CENTER_TOP,
            "Photo Booth Settings",
            FontId::proportional(title_size),
            Color32::WHITE,
        );

        let label_size = (h * 0.035).clamp(16.0, 28.0);
        let card = (w * 0.15).clamp(110.0, 200.0);
        let gap = (w * 0.045).clamp(20.0, 56.0);

        // --- Photos ---
        let photos_label_y = full.min.y + h * 0.22;
        ui.painter().text(
            Pos2::new(cx, photos_label_y),
            Align2::CENTER_CENTER,
            "Photos",
            FontId::proportional(label_size),
            Color32::from_gray(190),
        );
        let photos_y = photos_label_y + h * 0.045;
        let counts = [1usize, 2, 4];
        let total = counts.len() as f32 * card + (counts.len() as f32 - 1.0) * gap;
        let mut x = cx - total / 2.0;
        for &n in &counts {
            let rect = Rect::from_min_size(Pos2::new(x, photos_y), Vec2::splat(card));
            if photo_card(ui, rect, n, self.config.shots == n) {
                self.config.shots = n;
            }
            x += card + gap;
        }

        // --- Timer ---
        let timer_label_y = photos_y + card + h * 0.06;
        ui.painter().text(
            Pos2::new(cx, timer_label_y),
            Align2::CENTER_CENTER,
            "Timer",
            FontId::proportional(label_size),
            Color32::from_gray(190),
        );
        let timer_y = timer_label_y + h * 0.045;
        let secs = [3u64, 5, 7];
        let tcard_h = (card * 0.62).clamp(64.0, 130.0);
        let ttotal = secs.len() as f32 * card + (secs.len() as f32 - 1.0) * gap;
        let mut tx = cx - ttotal / 2.0;
        for &s in &secs {
            let rect = Rect::from_min_size(Pos2::new(tx, timer_y), Vec2::new(card, tcard_h));
            if timer_card(ui, rect, s, self.config.countdown_secs == s) {
                self.config.countdown_secs = s;
            }
            tx += card + gap;
        }

        // Done.
        let btn_w = (w * 0.3).clamp(180.0, 300.0);
        let btn_h = (h * 0.1).clamp(48.0, 76.0);
        let btn_rect = Rect::from_center_size(
            Pos2::new(cx, full.max.y - h * 0.05 - btn_h / 2.0),
            Vec2::new(btn_w, btn_h),
        );
        if place_button(ui, btn_rect, "Done", GREEN, true) {
            self.show_config = false;
        }
    }
}

/// Colour used to highlight the selected settings card.
const SELECT: Color32 = Color32::from_rgb(0, 120, 210);

/// A selectable card showing the photo-layout icon plus the shot count. Returns
/// whether it was tapped this frame.
fn photo_card(ui: &mut egui::Ui, rect: Rect, count: usize, selected: bool) -> bool {
    let resp = card_frame(ui, rect, ("photo-card", count), selected);
    let icon_r = rect.height() * 0.26;
    let icon_c = Pos2::new(rect.center().x, rect.top() + rect.height() * 0.40);
    count_icon(ui.painter(), icon_c, icon_r, count, Color32::BLACK, Color32::WHITE);
    ui.painter().text(
        Pos2::new(rect.center().x, rect.bottom() - rect.height() * 0.16),
        Align2::CENTER_CENTER,
        count.to_string(),
        FontId::proportional((rect.height() * 0.18).clamp(16.0, 30.0)),
        Color32::WHITE,
    );
    resp.clicked()
}

/// A selectable card showing a countdown duration (e.g. "5s").
fn timer_card(ui: &mut egui::Ui, rect: Rect, secs: u64, selected: bool) -> bool {
    let resp = card_frame(ui, rect, ("timer-card", secs), selected);
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        format!("{secs}s"),
        FontId::proportional((rect.height() * 0.42).clamp(20.0, 44.0)),
        Color32::WHITE,
    );
    resp.clicked()
}

/// Paint the shared rounded background/selection frame for a settings card and
/// return its click response.
fn card_frame(ui: &mut egui::Ui, rect: Rect, id: impl std::hash::Hash, selected: bool) -> egui::Response {
    let resp = ui.interact(rect, ui.id().with(id), egui::Sense::click());
    let fill = if selected {
        SELECT
    } else if resp.hovered() {
        Color32::from_rgb(52, 52, 64)
    } else {
        Color32::from_rgb(40, 40, 50)
    };
    ui.painter().rect_filled(rect, 14.0, fill);
    if selected {
        ui.painter().rect_stroke(
            rect,
            14.0,
            Stroke::new(3.0, Color32::WHITE),
            egui::StrokeKind::Inside,
        );
    }
    resp
}

/// Draw a black-on-white "photo count" glyph centred on `center`: a filled
/// circle of `bg`, with `count` rounded `fg` squares laid out the same way the
/// finished grid is (1 single, 2 vertically stacked, 4 in a 2x2).
fn count_icon(
    painter: &egui::Painter,
    center: Pos2,
    radius: f32,
    count: usize,
    fg: Color32,
    bg: Color32,
) {
    painter.circle_filled(center, radius, bg);
    let (cols, rows) = match count {
        0 | 1 => (1u32, 1u32),
        2 => (1, 2),
        _ => (2, 2),
    };
    // Square region the dots live in, comfortably inside the circle.
    let area = radius * 1.1;
    let gap = area * 0.14;
    let cell_w = (area - gap * (cols as f32 - 1.0)) / cols as f32;
    let cell_h = (area - gap * (rows as f32 - 1.0)) / rows as f32;
    let origin = center - Vec2::new(area, area) / 2.0;
    for i in 0..count.max(1) {
        let col = (i as u32 % cols) as f32;
        let row = (i as u32 / cols) as f32;
        let min = origin + Vec2::new(col * (cell_w + gap), row * (cell_h + gap));
        let cell = Rect::from_min_size(min, Vec2::new(cell_w, cell_h));
        painter.rect_filled(cell, cell_w.min(cell_h) * 0.18, fg);
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
    // Scale the glyph to the key; multi-char labels (".com", "Del") get a
    // smaller size so they don't overflow narrow keys.
    let factor = if label.chars().count() > 1 { 0.30 } else { 0.42 };
    let size = (rect.height() * factor).clamp(14.0, 34.0);
    let key = egui::Button::new(egui::RichText::new(label).size(size).color(Color32::WHITE))
        .fill(Color32::from_rgb(48, 48, 58))
        .corner_radius(10.0)
        .min_size(rect.size());
    ui.put(rect, key).clicked()
}

/// Minimal phone sanity check: only digits and common separators, with at
/// least 10 digits (a full US-style number). A leading `+` is allowed for
/// country codes.
fn valid_phone(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() {
        return false;
    }
    let digits = s.chars().filter(char::is_ascii_digit).count();
    let well_formed = s
        .chars()
        .enumerate()
        .all(|(i, c)| c.is_ascii_digit() || matches!(c, ' ' | '-' | '(' | ')') || (c == '+' && i == 0));
    well_formed && digits >= 10
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

#[cfg(test)]
mod tests {
    use super::{valid_email, valid_phone};

    #[test]
    fn phone_needs_ten_digits() {
        assert!(valid_phone("5551234567"));
        assert!(valid_phone("+1 (555) 123-4567"));
        assert!(!valid_phone("12345"));
        assert!(!valid_phone(""));
        assert!(!valid_phone("555-CALL-NOW")); // letters not allowed
        assert!(!valid_phone("1+5551234567")); // '+' only valid as a leading char
    }

    #[test]
    fn email_still_validates() {
        assert!(valid_email("a@b.com"));
        assert!(!valid_email("nope"));
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
fn draw_countdown(ui: &egui::Ui, rect: Rect, shot: usize, remaining: u32, total: usize) {
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
    progress_badge(ui, rect, shot, total);
}

/// "Photo N of M" badge at the top of the screen.
fn progress_badge(ui: &egui::Ui, rect: Rect, shot: usize, total: usize) {
    let painter = ui.painter();
    let pos = rect.center_top() + Vec2::new(0.0, 30.0);
    let text = format!("Photo {} of {}", shot + 1, total);
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
