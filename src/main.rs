//! Photobooth: live DSLR preview, a 3-2-1 countdown, four shots, and a 2x2
//! composite — driven by gphoto2 + egui.

mod app;
mod camera;
mod composite;
mod email;
mod outbox;

use app::PhotoboothApp;

fn main() -> eframe::Result {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([800.0, 600.0])
            .with_title("Photobooth"),
        ..Default::default()
    };

    eframe::run_native(
        "Photobooth",
        native_options,
        Box::new(|cc| Ok(Box::new(PhotoboothApp::new(cc)))),
    )
}
