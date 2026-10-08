# Reliability plan

What can go wrong on a call, what guards it now, and what is left. Built from an audit of the
74 calls handled by Callora's own agent between 2026-09-28 and 2026-10-08 (most of them the
owner's test calls, across many versions of the code):

| Seen in the calls | Count |
| --- | --- |
| Calls that ended with an order | 35 of 74 |
| The caller hung up in the middle of a booking | 18 |
| The same question twice in a row | 15 |
| "הלו, שומעים אותי?" (the line went quiet) | 22 |
| "סליחה, יש קצת רעש בקו" | 28 |
| The agent hung up on "ביי" without an order | 4 |
| The agent model timed out (4 s) or replied with broken JSON | 7 |
| The agent's decision | 1.36 s median, 2.3 s p90, up to 4 s |

## 1. Hearing the caller

| What goes wrong | Guard now | Measured |
| --- | --- | --- |
| A street or town the recognizer cannot spell | Second hearing by an audio model told the city's streets or the towns | streets 50% → 85% (past calls), 77% → 95% (random streets); towns 62% → 86% |
| A short answer heard as another word ("שתיים" as "ביי") | A passengers answer with no number is heard again, told a number is expected | the audio model hears "שתיים" in the recorded case |
| Background noise taken for speech, the end of a sentence heard late | VAD that follows the background and asks RNNoise whether a sound is a voice | end of speech 2.2 s → 0.3 s; noise alone heard as speech 16/21 → 7/21 |
| Someone near the caller | Words far quieter than the caller's own voice get a FAR VOICE note | 9 of 12 side-talk clips flagged; 8 of 542 callers' own utterances |
| The agent's own voice on a speakerphone | Three words or more, begun while the agent spoke, nearly all its own: dropped | |
| The line cutting out, a clipped microphone | The agent is told words may be missing and asks again | |
| A paused "אני רוצה." taken for noise | Held as the start of a request, joined with what follows | |

Measured and left off: the recognizer's own noise reduction (fewer of the caller's words),
RNNoise-cleaned audio to the recognizer (no gain), word confidence (as many right words
flagged as wrong ones), gpt-live-transcribe (no faster, other scripts), gpt-realtime-2.1
hearing the audio itself (no faster, misses details).

## 2. Silence and delay

| What goes wrong | Guard now |
| --- | --- |
| The agent model is slow | OpenAI priority processing: first words 1.35 s → 0.92 s median, slowest 2.82 s → 1.28 s; production eval 0.88 s median, 1.14 s p90. The backup model races it after 1.1 s |
| The agent waited on its own reasoning line | The read of the caller is written after the words (0.2 s) |
| A transcript arriving late was called "noise" | The no-words timer waits for the recognizer |
| The agent fails (timeout, broken reply) | The rules answer the turn |
| Recognition fails after its session opened (out of credit, a rejected key) | The call fails over to the backup recognizer for five minutes; the words the failed one never answered are sent to the backup |
| Silence while the agent decides | A thinking sound, played only on a known wait or after 1.5 s: built and tested, off until the owner approves a recording |

## 3. The conversation

| What goes wrong | Guard now |
| --- | --- |
| Questions out of the fixed order | Held and replaced by the next one, from the first turn |
| The same question twice in a row | The engine's repeats take other words; the agent's start with "סליחה, לא שמעתי טוב." |
| A lone "ביי"/"לא" ending a booking | Asked about once: "סליחה, לא שמעתי טוב." and the question again |
| Pickup and destination mixed | מ/ל decide which place is which |
| A street that is not in the list | The closest street offered; a place without an address taken as said, marked for the driver |
| Silence after "משהו נוסף?" on a sent ride | A goodbye, not "הלו? אני פה" |

| A returning caller says their address again | Their last ride's pickup is offered in the greeting ("שוב מבן זכאי 45 באלעד?") and a plain yes takes it; then its destination the same way |
| A pickup not found in the lists | The caller is texted a link that sends their phone's position into the call (needs `TWILIO_SMS_FROM`) |
| A town said alone ("בני ברק.") | "מבני ברק או לבני ברק?" |

## 4. Releases

- Every push to main runs fmt, clippy, the whole test suite, the agent eval with the real
  model, and the audio eval: 20 turns of a booking said by four voices over street noise,
  heard by the production recognizer and second hearing, then answered by the agent (each
  eval: pass 90% or nothing ships). The deploy waits for them.
- Every call's health is logged at its end ("call health") and counted in
  `callora_call_problems_total{problem}`: booking left unfinished, the same question twice,
  a hang-up in the middle of a booking, slow turns.

## 5. Left to do

Needs the owner (money or a decision):
- An SMS sender in Twilio for the location link (an alphanumeric sender ID such as "Callora",
  registered in Trust Hub, or an SMS-capable number), then the GitHub variable
  `TWILIO_SMS_FROM`. The call's own number cannot text.
- Calls through Twilio's European region (Ireland) instead of the US: the server is in
  Jerusalem, Twilio's US media is 180-270 ms away each way.
- OpenAI billing on auto-recharge: on 2026-10-08 the credit ran out and recognition stopped.
- A paid key for a second provider (Gemini or another): the fallback model when OpenAI fails
  is on a free tier of 5 requests a minute, so it cannot carry calls.
- A thinking sound to approve by ear before it is turned on.
- A daily health report sent to the owner (WhatsApp/Telegram).

Engineering:
- More audio cases: every live call that goes wrong becomes one (its audio, its scene).
