# Live-call voice tuning

Settings for how the agent sounds and behaves on a call. All are environment variables read by
`callora serve` (`apply_voice_env` in `crates/callora/src/main.rs`); defaults live in
`SessionConfig::default()` and `BargeConfig::default()`. Nothing here is a business-file setting
except the ElevenLabs voice settings (`voice.settings`, `voice.deliveries`).

## Loudness

| Variable | Default | Meaning |
|---|---|---|
| `TTS_GAIN_DB` | `4.0` | Gain on everything the agent says (library clips, cached and live TTS). `0` plays audio bit-exact. |
| `TTS_GAIN_MAX_DB` | `12.0` | Cap on base gain plus the caller's "speak louder" (+4 dB per request). |
| `TTS_LIMITER_CEILING_DBFS` | `-1.5` | Peak the limiter keeps the output under after gain. |

Gain is applied at playout (one place: `Session::enqueue`), so it never changes the voice library
or the TTS cache.

## Barge-in (the caller's voice over the agent)

A voice stops the agent when `barge::classify` says so (see the module doc). Listening sounds
("כן", "אהה") and noise only count through the plain "voice went on" rule.

| Variable | Default | Meaning |
|---|---|---|
| `BARGE_CONFIRM_MS` | `900` | A voice this long stops the agent whatever it says. |
| `BARGE_CONFIRM_GREETING_MS` | `1500` | The same over the greeting. |
| `BARGE_CONFIRM_READBACK_MS` | `1200` | The same during a read-back. |
| `BARGE_MIN_VOICED_MS` | `300` | Voice needed before real words count. |
| `BARGE_MIN_WORDS` | `2` | Real words needed (with `BARGE_MIN_VOICED_MS`). |
| `BARGE_SINGLE_WORD_MS` | `500` | Voice needed before a single word counts. |
| `BARGE_STRONG_MS` | `500` | A loud voice this long stops the agent without words. `0` turns the rule off. |
| `BARGE_STRONG_RMS_RATIO` | `2.5` | "Loud" = utterance mean level over this multiple of the VAD threshold. |
| `BARGE_FINAL_MIN_VOICED_MS` | `200` | A final transcript that arrives while the agent talks stops it only with real words and this much voice; otherwise it is background speech and is ignored. `0`: any final stops it. |
| `BARGE_LEGACY` | off | `true`: any words stop the agent at once, no loud rule (the old behaviour). |

## Sentence-end protection

| Variable | Default | Meaning |
|---|---|---|
| `SENTENCE_END_PROTECT_MS` | `400` | When an interruption lands with at most this much of the current sentence left (and its length is known), the sentence finishes; what was queued after it is dropped. `0` cuts at once. |

A live TTS sentence still arriving has an unknown end and is always cut.

## Endpointing and latency

| Variable | Default | Meaning |
|---|---|---|
| `VAD_ENDPOINT_MS` | `500` | Silence that ends an utterance (existing). |
| `VAD_ENDPOINT_SHORT_MS` | `350` | Used when the partial transcript is a finished short answer (≤3 words, and the call is waiting for a short answer: the read-back, "anything else?"). |
| `VAD_ENDPOINT_LONG_MS` | `700` | Used when the partial transcript looks broken off ("…", a lone letter). |
| `VAD_TRIGGER_MS` | `100` | Voice needed for the VAD to say speech started (existing). |
| `AGENT_SPECULATE` | on | Start the agent on the partial transcript (existing). |

Set both `VAD_ENDPOINT_SHORT_MS` and `VAD_ENDPOINT_LONG_MS` to `VAD_ENDPOINT_MS` to turn the
adaptation off.

## Gaps inside a reply

| Variable | Default | Meaning |
|---|---|---|
| `TTS_START_BUFFER_MS` | `250` | Speech a live sentence buffers before it starts, first of a reply. |
| `TTS_CONTINUATION_BUFFER_MS` | `120` | The same for a sentence that follows other audio of the reply. |
| `TTS_REBUFFER_MS` | `250` | Speech buffered after a stream ran dry mid-sentence. |
| `AUDIO_TRIM_SILENCE` | on | `false`: play the silence TTS puts at the ends of a sentence. |
| `AUDIO_TRIM_THRESHOLD_RMS` | `60` | A 10 ms window quieter than this is silence (about −55 dBFS; digital silence is 0, so soft consonants and fading word endings are kept). |
| `AUDIO_JOIN_PAD_MS` | `60` | Silence kept before the first sound. |
| `AUDIO_TRIM_TAIL_PAD_MS` | `100` | Silence kept after the last sound (word endings fade out). Only the silence at the two ends is ever cut, never anything inside a sentence. |
| `AUDIO_GAP_WARN_MS` | `120` | A hole this long inside a reply is logged as a warning. |
| `AUDIO_GAP_IGNORE_MS` | `3000` | A longer one is a pause (a reprompt), not a hole. |

## Metrics (`/metrics`)

`callora_agent_first_ms`, `callora_vad_endpoint_ms`, `callora_caller_speech_ms`,
`callora_audio_gap_ms`, `callora_audio_gaps_total{kind=seam|underrun}`,
`callora_barge_in_reason_total{reason=words|voiced_duration|loud_sustained|final_transcript|yes_over_readback}`,
`callora_barge_in_suppressed_total{reason=noise|backchannel|short}`,
`callora_barge_in_remaining_ms`, `callora_sentence_end_protected_total`, plus the existing
`callora_response_latency_ms`, `callora_stt_final_ms`, `callora_tts_first_chunk_ms`,
`callora_barge_in_latency_ms`.

Logs: `turn timing` (per reply: endpoint, caller speech ms and RMS, agent first words, TTS first
byte, first audio, reply from speech end and from the last sound), `barge-in: …` (reason,
voiced ms, RMS mean/peak, partial), `agent audio cut by the interruption` (remaining ms),
`interrupted near the end of the sentence`, `silence inside the agent's reply`.

## Voice A/B

```
callora voice-library ab --business taxi --out voice-ab [--model eleven_v3] [--preset name=stability[,style[,boost]]]
```

Synthesizes the same sentences for each preset (default: `current`, `natural` at stability 0.5,
`expressive` at 0.5 with style 0.25), applies tempo and gain as a call would, and writes WAV
files plus `voice-ab/index.html`. On `eleven_v3` stability snaps to creative 0.0 / natural 0.5 /
robust 1.0 (0.9 *is* robust); `eleven_v4_turbo` takes the value as is. The command prints the
value actually sent. Changing `voice.settings` regenerates the library (`voice-library build`).
