//! Email delivery of the finished composite over SMTP (via `lettre`).
//!
//! The image is sent as a normal MIME attachment, so no public hosting is
//! required. Configuration comes from the environment:
//!
//! - `SMTP_HOST`  — e.g. `smtp.gmail.com`
//! - `SMTP_USER`  — SMTP username
//! - `SMTP_PASS`  — SMTP password / app password
//! - `FROM_ADDR`  — sender, e.g. `Photobooth <booth@example.com>`
//! - `SMTP_PORT`  — optional, defaults to 587 (STARTTLS); 465 uses implicit TLS

use std::path::Path;

use lettre::message::header::ContentType;
use lettre::message::{Attachment, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};

/// SMTP settings resolved from environment variables.
struct Config {
    host: String,
    port: u16,
    user: String,
    pass: String,
    from: String,
    /// STARTTLS (port 587) vs. implicit TLS (port 465).
    starttls: bool,
}

impl Config {
    fn from_env() -> Result<Self, String> {
        let require = |key: &str| {
            std::env::var(key).map_err(|_| format!("{key} is not set — see src/email.rs for the required SMTP env vars"))
        };
        let port: u16 = std::env::var("SMTP_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(587);
        Ok(Self {
            host: require("SMTP_HOST")?,
            user: require("SMTP_USER")?,
            pass: require("SMTP_PASS")?,
            from: require("FROM_ADDR")?,
            port,
            starttls: port != 465,
        })
    }
}

/// Send the PNG at `image_path` as an attachment to `to`.
///
/// Blocking (network I/O) — run it off the egui thread.
pub fn send_photo(to: &str, image_path: &Path) -> Result<(), String> {
    let cfg = Config::from_env()?;

    let bytes = std::fs::read(image_path).map_err(|e| format!("reading photo: {e}"))?;
    let filename = image_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("photobooth.jpg")
        .to_string();
    let mime = match image_path.extension().and_then(|e| e.to_str()) {
        Some(e) if e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg") => "image/jpeg",
        Some(e) if e.eq_ignore_ascii_case("png") => "image/png",
        _ => "application/octet-stream",
    };

    let from = cfg
        .from
        .parse()
        .map_err(|e| format!("invalid FROM_ADDR ({}): {e}", cfg.from))?;
    let recipient = to
        .parse()
        .map_err(|_| format!("'{to}' is not a valid email address"))?;

    let attachment = Attachment::new(filename).body(
        bytes,
        ContentType::parse(mime).expect("derived MIME type is valid"),
    );
    let note = SinglePart::plain(
        "Thanks for visiting the photobooth! Your photo is attached.".to_string(),
    );

    let message = Message::builder()
        .from(from)
        .to(recipient)
        .subject("Your photobooth photo")
        .multipart(MultiPart::mixed().singlepart(note).singlepart(attachment))
        .map_err(|e| format!("building email: {e}"))?;

    let builder = if cfg.starttls {
        SmtpTransport::starttls_relay(&cfg.host)
    } else {
        SmtpTransport::relay(&cfg.host)
    }
    .map_err(|e| format!("SMTP setup for {}: {e}", cfg.host))?;

    let mailer = builder
        .port(cfg.port)
        .timeout(Some(std::time::Duration::from_secs(20)))
        .credentials(Credentials::new(cfg.user, cfg.pass))
        .build();

    mailer
        .send(&message)
        .map_err(|e| format!("sending failed: {e}"))?;
    Ok(())
}
