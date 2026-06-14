-- Send a file (and an optional greeting) to a phone number through the macOS
-- Messages app over iMessage.
--
-- Usage:  osascript send-imessage.applescript "<phone>" "<file path>" ["<greeting>"]
--
-- Requirements:
--   * Messages.app is signed in to an iMessage account.
--   * Whatever runs this (Terminal, the watcher, etc.) has been granted
--     Automation permission to control Messages
--     (System Settings → Privacy & Security → Automation).
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
		end if
		send theFile to targetBuddy
	end tell
end run
