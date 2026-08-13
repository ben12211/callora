import type { AgentConfig } from '../domain/models.js';
import { composeAgentInstructions } from './policy.js';

/**
 * Payload builders and narrow parsers for the two wire protocols that meet in the
 * bridge: Twilio Media Streams and the OpenAI Realtime GA API.
 *
 * Twilio phone audio is G.711 mu-law at 8 kHz, and the Realtime API accepts and emits
 * `audio/pcmu`, so both directions are bridged without transcoding.
 */

export const TWILIO_AUDIO_FORMAT = 'audio/pcmu' as const;
export const ASSISTANT_AUDIO_MARK = 'callora-assistant-audio' as const;

/**
 * Transcription of the caller's audio. It is not used to drive the conversation — the
 * model still hears the audio directly — only to make live calls observable in the logs.
 */
export const DEFAULT_TRANSCRIPTION_MODEL = 'gpt-4o-transcribe' as const;

/**
 * Telephony noise profile. Callers are on a handset close to their mouth over a narrow
 * 8 kHz channel, which is exactly what `near_field` is meant for; it cleans the audio
 * ahead of turn detection without altering the pcmu bridge or adding any transcoding.
 */
export const INPUT_NOISE_REDUCTION = 'near_field' as const;

/**
 * Short domain hint for the transcriber. It biases spelling and vocabulary toward
 * customer-service phone speech; it is never shown to the caller or the agent.
 */
export function transcriptionPrompt(locale: string): string {
  return transcriptionLanguage(locale) === 'he'
    ? 'שיחת טלפון של שירות לקוחות בעברית. תמלל דיבור טבעי, כולל מילים באנגלית שנאמרות בעברית, מספרי הזמנה, כתובות אימייל ומספרי טלפון. אל תשלים ואל תנחש מילים שלא נאמרו.'
    : 'A customer-service phone call. Transcribe natural speech, including order numbers, email addresses, and phone numbers. Do not complete or guess words that were not said.';
}

export interface RealtimeSessionOptions {
  agent: AgentConfig;
  /** Caller number in E.164, used only as conversation context. */
  callerNumber?: string | null;
  transcriptionModel?: string;
}

/**
 * Whisper-style language hint: the bare ISO-639-1 code from the agent's locale, so
 * `he-IL` becomes `he`. Anything unrecognised is omitted rather than guessed, which
 * leaves the transcriber in auto-detect mode.
 */
export function transcriptionLanguage(locale: string): string | undefined {
  const code = locale.trim().toLowerCase().split(/[-_]/)[0];
  return code && /^[a-z]{2}$/.test(code) ? code : undefined;
}

function transcriptionConfig(agent: AgentConfig, model: string): Record<string, unknown> {
  const language = transcriptionLanguage(agent.language);
  const prompt = transcriptionPrompt(agent.language);
  return language ? { model, language, prompt } : { model, prompt };
}

export const END_CALL_TOOL_NAME = 'end_call' as const;

/** Reasons the model may report; the server never accepts a call identifier from it. */
export const END_CALL_REASONS = [
  'caller_said_goodbye',
  'request_completed',
  'caller_requested_hangup',
  'off_topic_abuse',
  'caller_silent',
] as const;

/**
 * The only way the model can hang up. It deliberately has no `callSid` parameter: the
 * server terminates the call it authorized for this stream, so a hallucinated or
 * caller-supplied identifier can never reach the Twilio REST API.
 */
export const END_CALL_TOOL = {
  type: 'function',
  name: END_CALL_TOOL_NAME,
  description:
    'End the phone call after saying a short goodbye. Use when the caller says goodbye or thanks you and needs nothing else, asks to hang up, when their request is complete, or after repeated unrelated or abusive turns.',
  parameters: {
    type: 'object',
    properties: {
      reason: {
        type: 'string',
        enum: [...END_CALL_REASONS],
        description: 'Why the call is ending.',
      },
    },
    required: ['reason'],
    additionalProperties: false,
  },
} as const;

/** `session.update` for a speech-to-speech telephony session. */
export function buildSessionUpdate(options: RealtimeSessionOptions): Record<string, unknown> {
  const { agent, callerNumber, transcriptionModel = DEFAULT_TRANSCRIPTION_MODEL } = options;
  return {
    type: 'session.update',
    session: {
      type: 'realtime',
      model: agent.realtimeModel,
      instructions: composeAgentInstructions({ agent, callerNumber }),
      output_modalities: ['audio'],
      tools: [END_CALL_TOOL],
      tool_choice: 'auto',
      audio: {
        input: {
          format: { type: TWILIO_AUDIO_FORMAT },
          transcription: transcriptionConfig(agent, transcriptionModel),
          noise_reduction: { type: INPUT_NOISE_REDUCTION },
          turn_detection: {
            type: 'server_vad',
            threshold: 0.5,
            prefix_padding_ms: 300,
            silence_duration_ms: 500,
            interrupt_response: true,
          },
        },
        output: {
          format: { type: TWILIO_AUDIO_FORMAT },
          voice: agent.voice,
        },
      },
    },
  };
}

/** Asks the model to speak the configured greeting as the first turn. */
export function buildGreetingResponse(agent: AgentConfig): Record<string, unknown> {
  return {
    type: 'response.create',
    response: {
      instructions: `Greet the caller now by saying exactly: "${agent.greeting}". Then stop and wait.`,
    },
  };
}

/** Answers an `end_call` tool call so the model's conversation state stays consistent. */
export function buildEndCallToolOutput(callId: string, alreadyEnding: boolean): Record<string, unknown> {
  return {
    type: 'conversation.item.create',
    item: {
      type: 'function_call_output',
      call_id: callId,
      output: JSON.stringify(
        alreadyEnding
          ? { status: 'already_ending', instruction: 'The call is already ending. Say nothing further.' }
          : { status: 'ending', instruction: 'Say one short goodbye, then stop talking.' },
      ),
    },
  };
}

/** Final spoken turn before the call is terminated. */
export function buildFarewellResponse(): Record<string, unknown> {
  return {
    type: 'response.create',
    response: {
      instructions:
        'Say one short, warm goodbye sentence and nothing else. Do not ask a question and do not add any other information.',
    },
  };
}

/** First long silence: check once whether the caller is still on the line. */
export function buildStillThereResponse(): Record<string, unknown> {
  return {
    type: 'response.create',
    response: {
      instructions:
        'The caller has been silent for a while. Ask once, in one short sentence, whether they are still on the line. Do not repeat anything you said before.',
    },
  };
}

export function buildAudioAppend(payload: string): Record<string, unknown> {
  return { type: 'input_audio_buffer.append', audio: payload };
}

export function buildTruncate(itemId: string, audioEndMs: number): Record<string, unknown> {
  return {
    type: 'conversation.item.truncate',
    item_id: itemId,
    content_index: 0,
    audio_end_ms: Math.max(0, Math.round(audioEndMs)),
  };
}

export function buildTwilioMedia(streamSid: string, payload: string): Record<string, unknown> {
  return { event: 'media', streamSid, media: { payload } };
}

export function buildTwilioMark(streamSid: string, name = ASSISTANT_AUDIO_MARK): Record<string, unknown> {
  return { event: 'mark', streamSid, mark: { name } };
}

export function buildTwilioClear(streamSid: string): Record<string, unknown> {
  return { event: 'clear', streamSid };
}

export function parseJsonObject(raw: string): Record<string, unknown> | null {
  try {
    const parsed: unknown = JSON.parse(raw);
    return typeof parsed === 'object' && parsed !== null && !Array.isArray(parsed)
      ? (parsed as Record<string, unknown>)
      : null;
  } catch {
    return null;
  }
}

export function readString(source: Record<string, unknown>, key: string): string | undefined {
  const value = source[key];
  return typeof value === 'string' ? value : undefined;
}

export function readObjectArray(source: Record<string, unknown>, key: string): Record<string, unknown>[] {
  const value = source[key];
  return Array.isArray(value)
    ? value.filter(
        (entry): entry is Record<string, unknown> =>
          typeof entry === 'object' && entry !== null && !Array.isArray(entry),
      )
    : [];
}

export function readObject(source: Record<string, unknown>, key: string): Record<string, unknown> | undefined {
  const value = source[key];
  return typeof value === 'object' && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : undefined;
}
