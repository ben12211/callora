import type { AgentConfig } from '../domain/models.js';
import {
  buildAudioAppend,
  buildGreetingResponse,
  buildSessionUpdate,
  buildTruncate,
  buildTwilioClear,
  buildTwilioMark,
  buildTwilioMedia,
  parseJsonObject,
  readObject,
  readString,
} from './protocol.js';

/** Minimal duplex text channel; implemented by both WebSocket ends and by tests. */
export interface MessageChannel {
  send(payload: string): void;
  close(): void;
  onMessage(handler: (raw: string) => void): void;
  onClose(handler: () => void): void;
  onError(handler: (error: Error) => void): void;
}

export interface BridgeLogger {
  info(details: Record<string, unknown>, message: string): void;
  warn(details: Record<string, unknown>, message: string): void;
  error(details: Record<string, unknown>, message: string): void;
}

export interface BridgeOptions {
  twilio: MessageChannel;
  openai: MessageChannel;
  agent: AgentConfig;
  businessId: string;
  callSid: string;
  callerNumber?: string | null;
  logger: BridgeLogger;
  /** Called once the Twilio stream and, when known, the OpenAI session are identified. */
  onIdentifiers?: (identifiers: { streamSid: string | null; openaiSessionId: string | null }) => void;
}

/**
 * Bridges exactly one Twilio Media Stream to exactly one OpenAI Realtime session.
 *
 * The bridge owns no global state: every call gets its own instance, and closing either
 * side tears down the other exactly once.
 */
export class MediaStreamBridge {
  private streamSid: string | null = null;
  private openaiSessionId: string | null = null;
  private greeted = false;
  private closed = false;

  /** Timestamp (ms into the stream) of the newest inbound media frame. */
  private latestMediaTimestamp = 0;
  /** Stream timestamp at which the current assistant response started playing. */
  private responseStartTimestamp: number | null = null;
  private lastAssistantItemId: string | null = null;
  /** Assistant audio chunks handed to Twilio but not yet acknowledged by a mark. */
  private pendingMarks = 0;

  public constructor(private readonly options: BridgeOptions) {}

  public start(): void {
    const { twilio, openai, agent, businessId, callSid, callerNumber, logger } = this.options;

    twilio.onMessage((raw) => this.handleTwilioMessage(raw));
    twilio.onClose(() => {
      logger.info({ businessId, callSid, streamSid: this.streamSid }, 'Twilio media stream closed');
      this.close('twilio-closed');
    });
    twilio.onError((error) => {
      logger.error({ businessId, callSid, error: error.message }, 'Twilio media stream error');
      this.close('twilio-error');
    });

    openai.onMessage((raw) => this.handleOpenAiMessage(raw));
    openai.onClose(() => {
      logger.info({ businessId, callSid, openaiSessionId: this.openaiSessionId }, 'OpenAI realtime connection closed');
      this.close('openai-closed');
    });
    openai.onError((error) => {
      logger.error({ businessId, callSid, error: error.message }, 'OpenAI realtime connection error');
      this.close('openai-error');
    });

    openai.send(JSON.stringify(buildSessionUpdate({ agent, callerNumber })));
  }

  public close(reason: string): void {
    if (this.closed) {
      return;
    }
    this.closed = true;
    this.options.logger.info(
      {
        businessId: this.options.businessId,
        callSid: this.options.callSid,
        streamSid: this.streamSid,
        openaiSessionId: this.openaiSessionId,
        reason,
      },
      'Realtime call bridge closed',
    );
    try {
      this.options.twilio.close();
    } finally {
      this.options.openai.close();
    }
  }

  private handleTwilioMessage(raw: string): void {
    const message = parseJsonObject(raw);
    if (!message) {
      return;
    }

    switch (readString(message, 'event')) {
      case 'start': {
        const start = readObject(message, 'start');
        this.streamSid = readString(message, 'streamSid') ?? (start ? readString(start, 'streamSid') : null) ?? null;
        const startedCallSid = start ? readString(start, 'callSid') : undefined;
        if (startedCallSid && startedCallSid !== this.options.callSid) {
          this.options.logger.warn(
            { businessId: this.options.businessId, callSid: this.options.callSid },
            'Twilio stream CallSid does not match the authorized call; closing',
          );
          this.close('call-sid-mismatch');
          return;
        }
        this.options.logger.info(
          { businessId: this.options.businessId, callSid: this.options.callSid, streamSid: this.streamSid },
          'Twilio media stream started',
        );
        this.options.onIdentifiers?.({ streamSid: this.streamSid, openaiSessionId: this.openaiSessionId });
        return;
      }
      case 'media': {
        const media = readObject(message, 'media');
        const payload = media ? readString(media, 'payload') : undefined;
        const timestamp = media ? Number(media['timestamp']) : Number.NaN;
        if (Number.isFinite(timestamp)) {
          this.latestMediaTimestamp = timestamp;
        }
        if (payload) {
          this.options.openai.send(JSON.stringify(buildAudioAppend(payload)));
        }
        return;
      }
      case 'mark': {
        if (this.pendingMarks > 0) {
          this.pendingMarks -= 1;
        }
        return;
      }
      case 'stop': {
        this.close('twilio-stop');
        return;
      }
      default:
        return;
    }
  }

  private handleOpenAiMessage(raw: string): void {
    const message = parseJsonObject(raw);
    if (!message) {
      return;
    }
    const type = readString(message, 'type');

    switch (type) {
      case 'session.created':
      case 'session.updated': {
        const session = readObject(message, 'session');
        this.openaiSessionId = (session ? readString(session, 'id') : undefined) ?? this.openaiSessionId;
        if (type === 'session.created') {
          this.options.logger.info(
            {
              businessId: this.options.businessId,
              callSid: this.options.callSid,
              openaiSessionId: this.openaiSessionId,
              model: this.options.agent.realtimeModel,
            },
            'OpenAI realtime session created',
          );
        }
        this.options.onIdentifiers?.({ streamSid: this.streamSid, openaiSessionId: this.openaiSessionId });
        if (!this.greeted) {
          this.greeted = true;
          this.options.openai.send(JSON.stringify(buildGreetingResponse(this.options.agent)));
        }
        return;
      }
      case 'response.output_audio.delta':
      case 'response.audio.delta': {
        const delta = readString(message, 'delta');
        if (!delta || !this.streamSid) {
          return;
        }
        if (this.responseStartTimestamp === null) {
          this.responseStartTimestamp = this.latestMediaTimestamp;
        }
        this.lastAssistantItemId = readString(message, 'item_id') ?? this.lastAssistantItemId;
        this.options.twilio.send(JSON.stringify(buildTwilioMedia(this.streamSid, delta)));
        this.options.twilio.send(JSON.stringify(buildTwilioMark(this.streamSid)));
        this.pendingMarks += 1;
        return;
      }
      case 'input_audio_buffer.speech_started': {
        this.handleBargeIn();
        return;
      }
      case 'response.done': {
        this.responseStartTimestamp = null;
        this.lastAssistantItemId = null;
        return;
      }
      case 'error': {
        const error = readObject(message, 'error');
        this.options.logger.error(
          {
            businessId: this.options.businessId,
            callSid: this.options.callSid,
            openaiSessionId: this.openaiSessionId,
            code: error ? readString(error, 'code') : undefined,
            message: error ? readString(error, 'message') : undefined,
          },
          'OpenAI realtime error',
        );
        return;
      }
      default:
        return;
    }
  }

  /** Caller started speaking: drop queued assistant audio and rewind the model's item. */
  private handleBargeIn(): void {
    if (this.pendingMarks === 0 || this.responseStartTimestamp === null || !this.streamSid) {
      return;
    }

    if (this.lastAssistantItemId) {
      const elapsedMs = this.latestMediaTimestamp - this.responseStartTimestamp;
      this.options.openai.send(JSON.stringify(buildTruncate(this.lastAssistantItemId, elapsedMs)));
    }
    this.options.twilio.send(JSON.stringify(buildTwilioClear(this.streamSid)));

    this.pendingMarks = 0;
    this.lastAssistantItemId = null;
    this.responseStartTimestamp = null;
  }
}
