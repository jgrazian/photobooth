-- Send a file (and an optional greeting) to a phone number through the macOS
-- Messages app over iMessage, using the native Messages scripting API.
--
-- Usage:  osascript send-imessage.applescript "<phone>" "<file path>" ["<greeting>"]
--
-- IMPORTANT — file location: on recent macOS the native `send <file>` only
-- attaches reliably when the file lives in a location Messages is allowed to
-- read (e.g. ~/Pictures). Sending straight off a network share or from
-- arbitrary temp paths silently fails (the text sends, the image doesn't). The
-- watcher therefore stages a JPG copy under ~/Pictures and passes that path
-- here. No GUI scripting / Accessibility permission is needed with this route.
--
-- Requirements:
--   * Messages.app is signed in to an iMessage account.
--   * The app running this (Terminal / the watcher) has Automation permission
--     to control Messages (System Settings → Privacy & Security → Automation).
--   * To reach a non-iPhone (SMS) number, set up Text Message Forwarding on
--     your iPhone for this Mac; otherwise only iMessage-capable numbers work.

on run argv
	if (count of argv) < 2 then error "need: <phone> <file path> [greeting]"
	set targetPhone to item 1 of argv
	set filePath to item 2 of argv
	set greeting to ""
	if (count of argv) > 2 then set greeting to item 3 of argv

	set theFile to POSIX file filePath

	tell application "Messages"
		set targetService to 1st service whose service type = iMessage
		set targetBuddy to buddy targetPhone of targetService
		if greeting is not "" then
			send greeting to targetBuddy
			-- Messages drops a second send fired immediately after the first;
			-- give the greeting time to flush before sending the attachment,
			-- otherwise the image silently fails to attach.
			delay 2
		end if
		send theFile to targetBuddy
		-- Let the attachment hand off to Messages before this script exits, so
		-- the watcher doesn't move the staged copy out from under an in-flight
		-- send.
		delay 2
	end tell
end run
