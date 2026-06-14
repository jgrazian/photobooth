# Photobooth

A touchscreen photobooth that drives a tethered Nikon (or other gPhoto2-supported)
camera: live preview, a 3-2-1 countdown, four shots, then a 2×2 composite the guest
can **text to themselves** (default) or email.

Built with [`gphoto2`](https://crates.io/crates/gphoto2),
[`egui`](https://github.com/emilk/egui)/`eframe`, `image`, and `lettre`.

## Flow

1. **Take Photo** → live preview with a big 3-2-1 countdown overlay.
2. The camera autofocuses and captures — repeated **4 times**.
3. The shots are composited into a single 2×2 image, saved to `./captures/`.
4. **Send Photo** opens an on-screen keypad to collect a phone number (or, in
   email mode, an email address), delivers the composite, then returns to the
   Take Photo screen for the next guest.

## Delivery modes

Set with `PHOTOBOOTH_SEND_MODE`:

| Mode             | `PHOTOBOOTH_SEND_MODE` | What the booth does                                   |
| ---------------- | ---------------------- | ----------------------------------------------------- |
| **Phone (SMS)**  | `sms` *(default)*      | Drops the composite + number into an **outbox** folder for a Mac to text via iMessage. |
| **Email**        | `email`                | Sends the composite over SMTP (see below).            |

### Phone / iMessage delivery

The booth usually runs on Linux, which can't talk to iMessage — so it doesn't
send the text itself. In `sms` mode it writes each job into an outbox folder:

```
<outbox>/photobooth-<ts>.png      the composite
<outbox>/photobooth-<ts>.phone    the destination number (written last, atomically)
```

A companion watcher on a **Mac** (signed in to Messages) reads that folder and
sends each photo over iMessage. The outbox is shared between the two machines —
e.g. over **Samba**: the Linux booth exports the folder and the Mac mounts it.

> One-time share setup (both directions — Linux-hosts or Mac-hosts) is in
> [`docs/network-share-setup.md`](docs/network-share-setup.md), with a
> ready-to-edit [`docs/smb.conf.example`](docs/smb.conf.example). The app itself
> only writes to the local `PHOTOBOOTH_OUTBOX` path — it does not create the
> share.

Booth side:

```sh
# Point the app at the shared outbox (defaults to ./captures/outbox)
PHOTOBOOTH_OUTBOX=/srv/photobooth/outbox cargo run --release
```

Mac side — mount the share, then run the watcher (in `mac/`):

```sh
# e.g. mount the booth's Samba share at /Volumes/outbox via Finder → Connect to Server
PHOTOBOOTH_OUTBOX=/Volumes/outbox ./mac/imessage-watcher.sh
```

The watcher polls the folder, sends each `*.phone`/`*.png` pair via iMessage, and
moves finished jobs into `sent/` (or `failed/`). Configure it with
`PHOTOBOOTH_POLL_SECONDS` (default `3`) and `PHOTOBOOTH_GREETING` (the text sent
before the image; `""` to send only the image).

**Mac requirements / gotchas:**

- Messages.app must be signed in to iMessage.
- **Two permissions are required** for the app that runs the watcher (e.g.
  Terminal): **Automation** (to control Messages + System Events) and
  **Accessibility** (System Settings → Privacy & Security → Accessibility).
  Accessibility is needed because, on recent macOS (Sonoma/Sequoia/Tahoe), the
  clean `send <file> to buddy` AppleScript API silently fails to attach files —
  so the sender loads the image onto the clipboard and GUI-scripts Messages to
  paste and send it. Driving the UI requires Accessibility.
- Because the send drives the UI, it briefly takes over Messages. If steps race
  ahead of the UI on a slow machine, raise `PHOTOBOOTH_GUI_DELAY` (seconds per
  step, default `0.8`).
- To text a **non-iPhone** number you need **Text Message Forwarding** enabled on
  your iPhone for this Mac; otherwise only iMessage-capable numbers go through.
- **Staying awake:** the watcher re-execs itself under `caffeinate` so the Mac
  won't idle-sleep while it runs. Keep it on AC power for long events.

## Requirements

- Rust (edition 2024).
- A camera supported by libgphoto2, connected over USB and powered on.
- System libraries. On Fedora:

  ```sh
  sudo dnf install libgphoto2-devel clang-devel libxkbcommon-devel
  ```

  (`libgphoto2-devel` for the camera, `clang-devel` for the `-sys` bindgen step,
  `libxkbcommon-devel` for the window.)

### Email configuration

Set `PHOTOBOOTH_SEND_MODE=email` to collect an email address instead of a phone
number. Sending is SMTP via `lettre`; the image goes out as an attachment, so no
image hosting is needed. Configure with environment variables:

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

Or in email mode with SMTP configured:

```sh
PHOTOBOOTH_SEND_MODE=email \
SMTP_HOST=smtp.gmail.com SMTP_USER=you@gmail.com SMTP_PASS=app_password \
FROM_ADDR="Photobooth <you@gmail.com>" cargo run --release
```

Set `PHOTOBOOTH_DEBUG=1` to print the camera's full gPhoto2 config tree at
startup (useful for finding capture/autofocus key names on a given body).

## Composite layout

The finished image is a 2×2 grid on an off-white card with a white border
(the outer border is wider than the gaps between photos), the `assets/banner.png`
graphic in the bottom-left, and an optional caption across the bottom border.

| Variable                      | What it does                                       |
| ----------------------------- | -------------------------------------------------- |
| `PHOTOBOOTH_BANNER_TEXT`      | caption text, in black, centred along the bottom border (unset/blank ⇒ no caption) |
| `PHOTOBOOTH_BANNER_FONT_SIZE` | caption pixel size (default `48`)                  |
| `PHOTOBOOTH_BANNER_FONT`      | path to a `.ttf`/`.otf` font (default: bundled `assets/Tangerine-Regular.ttf`) |

To preview the layout without a camera, render a template — black boxes in
place of the four photos — to `./composite-template.jpg`:

```sh
PHOTOBOOTH_BANNER_TEXT="Joey's Wedding" cargo run -- --template
```

## Notes

- On connect the camera is set, best-effort, to a small JPEG captured to internal
  RAM (faster transfers, no SD-card write). Full resolution is preserved in the
  saved composite.
- Autofocus is primed when a shoot begins and fired again right before each
  shutter release. It only works if the lens/body is in an AF mode.
- **Linux gotcha:** GNOME's `gvfs` auto-mounts cameras and locks the USB device.
  If the app reports "could not claim the USB device", run `gio mount -s gphoto2`
  (or `pkill -f gvfsd-gphoto2`) and hit **Retry**.
- Each session is saved to its own folder `./captures/photobooth-<timestamp>/`
  containing the original camera files (`shot-1.jpg` … `shot-4.jpg`) and the
  final `composite.png` (`./captures/` is git-ignored).
