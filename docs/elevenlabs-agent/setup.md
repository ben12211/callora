# The taxi agent on ElevenLabs

The system prompt is [`system_prompt.md`](system_prompt.md). It is built on ElevenLabs' own
prompt structure (Personality, Environment, Tone, Goal, Guardrails, Tools), with the taxi
business's rules from `businesses/taxi.json`, and example conversations for the model to copy
instead of a list of fixed answers. The agent is chosen on Callora's settings page
("מי עונה לטלפון") by its `agent_…` id.

## Agent settings

| Setting | Value | Why |
| --- | --- | --- |
| Language | Hebrew | |
| First message | `אהלן, מוניות קלורה, איך אפשר לעזור?` | Short, and lets the caller say where to |
| LLM | Gemini 3.8 Flash (or the best Gemini Flash the account lists) | The owner's choice; thinking low, temperature default |
| Voice | Itai (`JIxTgeeS5w0UQyBxEnrl`), the voice the Callora agent uses | Same sound on both paths |
| Text normalization | `elevenlabs` (after the model), not the prompt | Numbers, times and addresses read right even when the model writes digits |
| Turn eagerness | Normal; raise only if callers are cut off or the agent waits too long | |
| Soft timeout | On, with the filler `רגע, בודק.` | Fills a slow tool or model turn, as a person would |
| Interruptions | On | A caller who talks over the agent stops it |
| Max call length | 10 minutes | A taxi call is under two |
| Custom vocabulary / ASR keywords | Israeli cities of the service area, `בני ברק`, `אלעד`, `ביתר עילית`, `מודיעין עילית`, `בית שמש`, `נתב״ג`, `בנייני האומה`, `תחנה מרכזית`, `סינמה סיטי` | Biases recognition toward the names callers say |

## Knowledge base

Do not paste long lists into the prompt (latency, and the model follows a short prompt better).
Add as documents what only needs looking up: the places and streets a caller names in the
service area, and how they are pronounced.

## Tools

`transfer_to_number` (the dispatch desk's number) and `end_call` are ElevenLabs' system tools.
`create_ride` and `get_price` are server tools (webhooks) that Callora serves at
`POST {PUBLIC_BASE_URL}/webhooks/elevenlabs/tools/create-ride` and `.../get-price`. They run the
same actions as Callora's own agent (dispatch, the price list), put the ride on the orders page and
send it to WhatsApp and Telegram. Both need the header `x-callora-tools-token`: the code and the URL
are shown on the settings page ("מי עונה לטלפון", with ElevenLabs chosen).

`create_ride` body (JSON): `pickup` (string: street, number if said, city, e.g. `בן זכאי 45, אלעד`),
`destination` (string, same), `passengers` (integer), `customer_name` (string, Hebrew letters),
`notes` (string, may be empty), and from ElevenLabs' dynamic variables `conversation_id`
(`system__conversation_id`) and `caller_number` (`system__caller_id`). It returns
`{"ok": true, "message": "הנסיעה נשלחה"}`, or `ok: false` with a sentence to say; a ride told twice
in one conversation is sent once.

`get_price` body: `price_from` (city), `price_to` (city), `passengers` (integer, optional),
`round_trip` (boolean, optional). It returns `{"ok": true, "message": "<the sentence, in words>"}`.

## Test it

Test in ElevenLabs' own test panel with the cases in [`evaluation/agent`](../../evaluation/agent):
a request with everything at once, a landmark (`כניסה לעיר בביתר`), a frustrated caller
(`כמה פעמים אני צריך להגיד לך`), a noisy line, small talk with a request, an off-topic question
and a `כן אבל…`. Then call the number with the option chosen on the settings page.
