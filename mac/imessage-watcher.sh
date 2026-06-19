#!/usr/bin/env bash
#
# Watch the photobooth outbox folder (a Samba share mounted from the Linux
# booth) and deliver each queued photo to its phone number through the macOS
# Messages app over iMessage.
#
# Job protocol (written by the booth — see src/outbox.rs):
#   <id>.png     the composite image
#   <id>.phone   a text file whose first line is the destination number; it is
#                renamed into place only AFTER the .png is fully written, so the
#                presence of a .phone file means the whole job is ready.
#
# For each <id>.phone we send <id>.png to that number, then move both files into
# sent/ (or failed/ on error) so they are processed exactly once.
#
# Configuration (environment variables):
#   PHOTOBOOTH_OUTBOX         outbox folder to watch   (default: /Volumes/outbox)
#   PHOTOBOOTH_POLL_SECONDS   poll interval in seconds (default: 3)
#   PHOTOBOOTH_GREETING       text sent before the image ("" to send no text)
#
# Staying awake:
#   This script re-execs itself under `caffeinate` so the Mac won't idle-sleep
#   while the loop runs. Keep it on AC power for long events.

set -uo pipefail

# Re-exec under caffeinate so idle/display/disk/system sleep is held off while
# the loop runs.
if [[ "${PHOTOBOOTH_CAFFEINATED:-}" != "1" ]]; then
	exec env PHOTOBOOTH_CAFFEINATED=1 caffeinate -dimsu "$0" "$@"
fi

OUTBOX="${PHOTOBOOTH_OUTBOX:-/Volumes/outbox}"

# A URL (e.g. smb://host/outbox) is not a filesystem path — the shell tools below
# would silently create literal "smb:" junk dirs and watch an empty path. Mount
# the share first and point at the mount, e.g. /Volumes/outbox.
if [[ "$OUTBOX" == *://* ]]; then
	echo "ERROR: PHOTOBOOTH_OUTBOX looks like a URL ($OUTBOX). Mount the share first and point at the mount path, e.g. /Volumes/outbox" >&2
	exit 1
fi
POLL_SECONDS="${PHOTOBOOTH_POLL_SECONDS:-3}"
GREETING="${PHOTOBOOTH_GREETING:-Thanks for visiting the photobooth! Here is your photo 📸}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SENDER="$SCRIPT_DIR/send-imessage.applescript"

# Local staging dir for images about to be sent. MUST be a location Messages is
# allowed to read — ~/Pictures works; network shares and arbitrary temp paths do
# not (the native send silently fails to attach). Images are also converted to
# JPG here, which Messages attaches more reliably than PNG.
STAGE_DIR="${PHOTOBOOTH_STAGE_DIR:-$HOME/Pictures/photobooth-staging}"

log() { printf '%s  %s\n' "$(date '+%Y-%m-%d %H:%M:%S')" "$*"; }

if [[ ! -f "$SENDER" ]]; then
	log "ERROR: sender script not found at $SENDER"
	exit 1
fi

mkdir -p "$OUTBOX/sent" "$OUTBOX/failed" || {
	log "ERROR: cannot create sent/ and failed/ under $OUTBOX (is the share mounted?)"
	exit 1
}
mkdir -p "$STAGE_DIR" || {
	log "ERROR: cannot create staging dir $STAGE_DIR"
	exit 1
}

process_one() {
	local phone_file="$1"
	local base img phone
	base="$(basename "$phone_file" .phone)"
	img="$OUTBOX/$base.jpg"
	phone="$(head -n1 "$phone_file" | tr -d '[:space:]')"

	if [[ -z "$phone" ]]; then
		log "SKIP $base: empty phone number"
		mv -f "$phone_file" "$OUTBOX/failed/" 2>/dev/null
		return
	fi
	if [[ ! -f "$img" ]]; then
		# .phone exists but image missing — shouldn't happen given the write
		# order, but never delete a number we can't fulfil.
		log "WAIT $base: image $base.jpg not present yet"
		return
	fi

	# Messages can't attach a file straight off the network share, so stage a
	# copy under ~/Pictures (a location Messages is allowed to read). The booth
	# already produces JPG, so no conversion is needed.
	#
	# Copy to a temp name first, then verify the staged copy is complete (same
	# byte count as the source, and non-zero) before renaming it into place and
	# sending. A flaky SMB read can leave a truncated file even when cp returns
	# success; without this check Messages would attach a partial/corrupt image.
	# Retry an incomplete copy a few times before giving up — transient share
	# hiccups usually clear on a second attempt.
	local staged="$STAGE_DIR/$base.jpg"
	local staging="$staged.partial"
	local src_size staged_size attempt staged_ok=
	for attempt in 1 2 3; do
		rm -f "$staging"
		if ! cp "$img" "$staging" 2>/dev/null; then
			log "RETRY $base: copy to $staging failed (attempt $attempt/3)"
			continue
		fi
		src_size="$(wc -c <"$img" 2>/dev/null | tr -d ' ')"
		staged_size="$(wc -c <"$staging" 2>/dev/null | tr -d ' ')"
		if [[ -n "$staged_size" && "$staged_size" -ne 0 && "$staged_size" == "$src_size" ]]; then
			staged_ok=1
			break
		fi
		log "RETRY $base: staged copy incomplete (src=${src_size:-?} staged=${staged_size:-?} bytes, attempt $attempt/3)"
	done
	if [[ -z "$staged_ok" ]]; then
		log "FAIL $base: could not stage a complete local copy after 3 attempts"
		rm -f "$staging"
		mv -f "$img" "$phone_file" "$OUTBOX/failed/" 2>/dev/null
		return
	fi
	mv -f "$staging" "$staged"
	log "SEND $base -> $phone (staged $staged, $staged_size bytes)"

	local err
	err="$(osascript "$SENDER" "$phone" "$staged" "$GREETING" 2>&1 >/dev/null)"
	local rc=$?
	if [[ $rc -eq 0 ]]; then
		rm -f "$staged"
		mv -f "$img" "$phone_file" "$OUTBOX/sent/" 2>/dev/null
		log "OK   $base"
	else
		# Leave the staged copy in place so it can be inspected after a failure.
		mv -f "$img" "$phone_file" "$OUTBOX/failed/" 2>/dev/null
		log "FAIL $base: ${err:-osascript error} (staged copy kept at $staged)"
	fi
}

log "watching $OUTBOX every ${POLL_SECONDS}s (sent/ + failed/ in use). Ctrl-C to stop."
while true; do
	shopt -s nullglob
	for f in "$OUTBOX"/*.phone; do
		process_one "$f"
	done
	shopt -u nullglob
	sleep "$POLL_SECONDS"
done
