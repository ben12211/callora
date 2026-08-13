// Development-only caller allowlist.
//
// Copy this file to `allowlist.local.js` (gitignored) and add the numbers you test
// from. On every incoming call Callora checks Twilio's `From` against this list and
// rejects callers that are not on it, before any OpenAI Realtime session is opened.
//
// Notes:
// - Numbers must be E.164. Spaces, dashes, and parentheses are stripped for you,
//   so "+972 50-123-4567" is fine, but a missing country code is not.
// - An empty list — or no `allowlist.local.js` at all — disables the allowlist and
//   allows every caller. That is the production behaviour: the file is never deployed.
// - `From` is used for this check only. Business routing always uses Twilio `To`.

export const allow_list = [];
