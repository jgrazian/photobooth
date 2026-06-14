//! Outbox delivery for **SMS / iMessage mode**.
//!
//! The booth typically runs on a Linux box, which cannot talk to iMessage. So
//! instead of sending anything itself, in this mode the booth drops the finished
//! composite plus the destination phone number into a shared "outbox" folder
//! (e.g. a Samba share). A companion watcher running on a Mac
//! (see `mac/imessage-watcher.sh`) picks each job up and sends it through the
//! Messages app over iMessage.
//!
//! Protocol — designed so the watcher never sees a half-written job:
//!
//!   1. `<id>.jpg`   — the composite, copied in and flushed first.
//!   2. `<id>.phone` — the destination number. Written to `<id>.phone.tmp` and
//!                     then **renamed** into place. The rename is atomic, so the
//!                     `.phone` sidecar only appears once the image is fully on
//!                     disk.
//!
//! The watcher globs for `*.phone`, derives the matching `<id>.jpg`, sends, and
//! then moves both files into `sent/` (or `failed/`).

use std::path::{Path, PathBuf};

/// Outbox directory. Override with `PHOTOBOOTH_OUTBOX`; defaults to
/// `./captures/outbox` (point this at the Samba-shared folder in production).
fn outbox_dir() -> PathBuf {
    std::env::var_os("PHOTOBOOTH_OUTBOX")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("captures").join("outbox"))
}

/// Queue the composite at `image_path` for delivery to `phone`.
///
/// Cheap (local file I/O) but kept on the same background-send path as email so
/// the UI flow is identical.
pub fn queue(phone: &str, image_path: &Path) -> Result<(), String> {
    let dir = outbox_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("create outbox {}: {e}", dir.display()))?;

    let id = job_id(image_path);

    // 1. Copy the composite in first, fully, so it's complete on disk before
    //    the sidecar that points at it appears. Keep the source extension
    //    (normally `jpg`) so the watcher can attach it without converting.
    let ext = image_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("jpg");
    let img_dst = dir.join(format!("{id}.{ext}"));
    std::fs::copy(image_path, &img_dst)
        .map_err(|e| format!("copy composite to outbox: {e}"))?;

    // 2. Write the number to a temp file, then atomically rename it into place.
    let phone_final = dir.join(format!("{id}.phone"));
    let phone_tmp = dir.join(format!("{id}.phone.tmp"));
    std::fs::write(&phone_tmp, format!("{}\n", phone.trim()))
        .map_err(|e| format!("write phone sidecar: {e}"))?;
    std::fs::rename(&phone_tmp, &phone_final)
        .map_err(|e| format!("finalize phone sidecar: {e}"))?;

    Ok(())
}

/// Derive a stable, unique-ish job id from the composite's session directory
/// name (`captures/photobooth-<ts>/composite.png` → `photobooth-<ts>`), falling
/// back to the file stem if there's no parent directory.
fn job_id(image_path: &Path) -> String {
    image_path
        .parent()
        .and_then(Path::file_name)
        .or_else(|| image_path.file_stem())
        .and_then(|n| n.to_str())
        .unwrap_or("photobooth-job")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_id_uses_session_dir_name() {
        let p = Path::new("captures/photobooth-1234/composite.png");
        assert_eq!(job_id(p), "photobooth-1234");
    }

    #[test]
    fn queue_writes_image_and_phone_sidecar() {
        // Isolate the outbox to a unique temp dir for this test.
        let dir = std::env::temp_dir().join("photobooth-outbox-test-queue");
        let _ = std::fs::remove_dir_all(&dir);
        unsafe { std::env::set_var("PHOTOBOOTH_OUTBOX", &dir) };

        // A fake composite in a session-style directory.
        let session = std::env::temp_dir().join("photobooth-9999");
        std::fs::create_dir_all(&session).unwrap();
        let composite = session.join("composite.jpg");
        std::fs::write(&composite, b"fake jpg bytes").unwrap();

        queue(" +1 555 123 4567 ", &composite).unwrap();

        let img = dir.join("photobooth-9999.jpg");
        let phone = dir.join("photobooth-9999.phone");
        assert_eq!(std::fs::read(&img).unwrap(), b"fake jpg bytes");
        // Number is trimmed and newline-terminated; no temp file left behind.
        assert_eq!(std::fs::read_to_string(&phone).unwrap(), "+1 555 123 4567\n");
        assert!(!dir.join("photobooth-9999.phone.tmp").exists());

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&session);
    }
}
