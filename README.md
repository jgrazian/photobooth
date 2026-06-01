# Photobooth

A touchscreen photobooth that drives a tethered Nikon (or other gPhoto2-supported)
camera: live preview, a 3-2-1 countdown, four shots, then a 2×2 composite the guest
can email to themselves.

Built with [`gphoto2`](https://crates.io/crates/gphoto2),
[`egui`](https://github.com/emilk/egui)/`eframe`, `image`, and `lettre`.

## Flow

1. **Take Photo** → live preview with a big 3-2-1 countdown overlay.
2. The camera autofocuses and captures — repeated **4 times**.
3. The shots are composited into a single 2×2 image, saved to `./captures/`.
4. **Send Photo** opens an on-screen keyboard to email the composite as an
   attachment, then returns to the Take Photo screen for the next guest.

## Requirements

- Rust (edition 2024).
- A camera supported by libgphoto2, connected over USB and powered on.
- System libraries. On Fedora:

  ```sh
  sudo dnf install libgphoto2-devel clang-devel libxkbcommon-devel
  ```

  (`libgphoto2-devel` for the camera, `clang-devel` for the `-sys` bindgen step,
  `libxkbcommon-devel` for the window.)

## Email configuration

Sending is SMTP via `lettre`; the image goes out as an attachment, so no image
hosting is needed. Configure with environment variables:

| Variable        | Notes                                              |
| --------------- | -------------------------------------------------- |
| `SMTP_HOST`     | e.g. `smtp.gmail.com`                              |
| `SMTP_USER`     | SMTP username                                      |
| `SMTP_PASS`     | password / app password (Gmail requires an App Password) |
| `FROM_ADDR`     | e.g. `Photobooth <booth@example.com>`             |
| `SMTP_PORT`     | optional; default `587` (STARTTLS), `465` = implicit TLS |

## Running

```sh
cargo run --release
```

Or with email configured:

```sh
SMTP_HOST=smtp.gmail.com SMTP_USER=you@gmail.com SMTP_PASS=app_password \
FROM_ADDR="Photobooth <you@gmail.com>" cargo run --release
```

Set `PHOTOBOOTH_DEBUG=1` to print the camera's full gPhoto2 config tree at
startup (useful for finding capture/autofocus key names on a given body).

## Notes

- On connect the camera is set, best-effort, to a small JPEG captured to internal
  RAM (faster transfers, no SD-card write). Full resolution is preserved in the
  saved composite.
- Autofocus is primed when a shoot begins and fired again right before each
  shutter release. It only works if the lens/body is in an AF mode.
- **Linux gotcha:** GNOME's `gvfs` auto-mounts cameras and locks the USB device.
  If the app reports "could not claim the USB device", run `gio mount -s gphoto2`
  (or `pkill -f gvfsd-gphoto2`) and hit **Retry**.
- Saved composites land in `./captures/` (git-ignored).
