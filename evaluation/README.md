# Evaluation

Two corpora live here:

| Path | What it checks | Runs in `cargo test` |
| --- | --- | --- |
| `hebrew-utterances.json` | Spoken-text normalization: no digit or symbol reaches the TTS | yes |
| `agent/*.json` | The conversation agent, with the **real model**, on conversations that went wrong on live calls | only their format; the run needs `OPENAI_API_KEY` |

## Running the agent eval

```bash
./dev callora eval                                   # the production agent (AGENT_MODEL hedged by AGENT_BACKUP_MODEL)
./dev callora eval --model gpt-6-sol --model gpt-4o  # compare models on the same cases
./dev callora eval --model gpt-6-sol --reasoning low # the same model, thinking a little
./dev callora eval --only betar --repeat 10          # one case, ten times
./dev callora eval --json target/eval.json           # the full report: every reply and failure
./dev callora eval --check                           # the case files only, no model
```

Each case runs `--repeat` times (default 3), because the model is not deterministic: a case
that passes 2 of 3 is marked FLAKY, which is usually a prompt that leaves room for doubt.
The report gives, per model, the pass rate, the time to the first words the caller could
hear and to the complete decision (p50/p90), and the tokens. With prices in
`EVAL_PRICES="gpt-6-sol=2/0.2/10,gpt-6-luna=0.1/0.01/0.5"` (dollars per million input /
cached input / output tokens) it also gives the cost per turn.

A case runs through the same engine, fast lane, place checks (Israel's streets list) and
business rules as a live call. Actions use their **mock** backends only, so an eval never
books a real ride, and a handoff number is assumed so transfers behave as in production.

Before changing the agent's prompt (`crates/callora-core/src/agent.rs`), its rules
(`businesses/taxi.json` → `agent`) or the model, run the eval before and after. A change
that fixes one case and breaks two others shows up here instead of on a caller.

`agent/tone.json` holds the cases for tone and context: a complaint inside small talk, a caller
repeating themselves, sarcasm, a joke, impatience, and no opening "הבנתי"/"כמובן". Their expectations were
written without a model run: run them, and loosen a pattern that fails on a good answer.

## Adding a case

Every conversation that goes wrong on a live call becomes a case. Open the call on the
calls page (`/calls`), press **הורדה כמקרה בדיקה (eval)**, and save the file under `agent/`
(or add it to one of the existing files). Then write what should have happened:

```json
[{
  "id": "betar_is_betar_illit",
  "note": "live call 2026-09-26 booked \"רימון 16, מיתר\"",
  "turns": [
    { "caller": "צריך מונית",
      "scripted": { "action": "none", "task": "book_ride", "fields": [], "say": "סבבה, מאיזו עיר לאסוף?" } },
    { "caller": "מביתר",
      "expect": { "cities": { "pickup": "ביתר עילית" }, "says_any": ["רחוב"] } }
  ]
}]
```

- `caller` is what the recognizer heard; `second_hearing` what a second transcription heard.
- A `scripted` turn is a fixed decision (`action`, `task`, `fields`, `asks`, `say`, `phrase`) that
  sets the scene without calling the model. Only the unscripted turns are tested.
- `customer` (on the case) is a known caller, as the customer lookup would return them.

What a turn can `expect` (every field is optional; `a|b` accepts either):

| Field | Meaning |
| --- | --- |
| `action` | The model's action is one of these: `none`, `read_back`, `submit`, `transfer`, `end_call` |
| `task` | The task the model names (`""` for none) |
| `fields` | Details the model passes this turn: slot → text in the value |
| `no_fields` | Slots the model must not pass this turn (it would be inventing them) |
| `says_any` / `says_none` | What the caller hears this turn contains one of / none of these |
| `slots` | Stored details after the turn: slot → text in the value (`""` = any value) |
| `missing` | Slots still without a value |
| `cities` | A place whose city is noted while its street is still asked: slot → city |
| `step` | `none`, `collecting`, `confirming_slot`, `awaiting_confirmation` or `executing` |
| `submitted`, `ended`, `handoff` | A business action ran / the call hung up / went to a person, this turn |

Expect what matters for the bug, not the exact wording: a good dispatcher can say the same
thing several ways, and a case that pins one sentence fails on a correct answer.
