//! Photobooth: live DSLR preview, a countdown, a guest-chosen number of shots
//! (1, 2, or 4), and a composite — driven by gphoto2 + egui.

mod app;
mod camera;
mod composite;
mod email;
mod outbox;

use app::PhotoboothApp;

fn main() -> eframe::Result {
    // `--template [count]`: render just the composite layout (black boxes for
    // photos, plus the banner/caption) for the given shot count (default 4) to
    // ./composite-template.jpg and exit — a quick way to iterate on the layout
    // without a camera.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--template") {
        let count = args.iter().find_map(|a| a.parse::<usize>().ok()).unwrap_or(4);
        match composite::render_template(count) {
            Ok(path) => println!("wrote composite template to {}", path.display()),
            Err(e) => eprintln!("template error: {e}"),
        }
        return Ok(());
    }

    // PHOTOBOOTH_FULLSCREEN (any non-empty value) launches straight into
    // fullscreen — handy for kiosk/event use. F11 still toggles it at runtime.
    let start_fullscreen = std::env::var_os("PHOTOBOOTH_FULLSCREEN")
        .is_some_and(|v| !v.is_empty());

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([800.0, 600.0])
            .with_fullscreen(start_fullscreen)
            .with_title("Photobooth"),
        ..Default::default()
    };

    eframe::run_native(
        "Photobooth",
        native_options,
        Box::new(|cc| Ok(Box::new(PhotoboothApp::new(cc)))),
    )
}
