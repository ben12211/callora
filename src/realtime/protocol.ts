import type { AgentConfig } from '../domain/models.js';

/**
 * Payload builders and narrow parsers for the two wire protocols that meet in the
 * bridge: Twilio Media Streams and the OpenAI Realtime GA API.
 *
 * Twilio phone audio is G.711 mu-law at 8 kHz, and the Realtime API accepts and emits
 * `audio/pcmu`, so both directions are bridged without transcoding.
 */

export const TWILIO_AUDIO_FORMAT = 'audio/pcmu' as const;
export const ASSISTANT_AUDIO_MARK = 'callora-assistant-audio' as const;

export interface RealtimeSessionOptions {
  agent: AgentConfig;
  /** Caller number in E.164, used only as conversation context. */
  callerNumber?: string | null;
}

function sessionInstructions(agent: AgentConfig, callerNumber?: string | null): string {
  const lines = [
    agent.instructions,
    `Always speak in ${agent.language}.`,
    'Keep answers short and natural for a phone conversation.',
  ];
  if (callerNumber) {
    lines.push(`The caller is phoning from ${callerNumber}.`);
  }
  return lines.join('\n');
}

/** `session.update` for a speech-to-speech telephony session. */
export function buildSessionUpdate(options: RealtimeSessionOptions): Record<string, unknown> {
  const { agent, callerNumber } = options;
  return {
    type: 'session.update',
    session: {
      type: 'realtime',
      model: agent.realtimeModel,
      instructions: sessionInstructions(agent, callerNumber),
      output_modalities: ['audio'],
      audio: {
        input: {
          format: { type: TWILIO_AUDIO_FORMAT },
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

export function readObject(source: Record<string, unknown>, key: string): Record<string, unknown> | undefined {
  const value = source[key];
  return typeof value === 'object' && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : undefined;
}
