import { describe, expect, it } from 'vitest';
import { MAX_LOGGED_TRANSCRIPT_CHARS, truncateTranscript } from '../src/realtime/bridge.js';
import {
  DEFAULT_TRANSCRIPTION_MODEL,
  buildSessionUpdate,
  transcriptionLanguage,
} from '../src/realtime/protocol.js';
import {
  RecordingLogger,
  agent,
  businessId,
  callSid,
  openStream,
  startBridge,
  streamSid,
} from './support/realtime-harness.js';

const callId = '11111111-1111-4111-8111-111111111111';

interface SessionAudioInput {
  session: {
    audio: { input: { transcription?: { model: string; language?: string } } };
  };
}

describe('input audio transcription configuration', () => {
  it('enables transcription with a Hebrew hint for a he-IL agent', () => {
    const update = buildSessionUpdate({ agent }) as unknown as SessionAudioInput;

    expect(update.session.audio.input.transcription).toEqual({
      model: DEFAULT_TRANSCRIPTION_MODEL,
      language: 'he',
    });
  });

  it('accepts an override model and derives the language from the locale', () => {
    const update = buildSessionUpdate({
      agent: { ...agent, language: 'en-US' },
      transcriptionModel: 'gpt-4o-transcribe',
    }) as unknown as SessionAudioInput;

    expect(update.session.audio.input.transcription).toEqual({
      model: 'gpt-4o-transcribe',
      language: 'en',
    });
  });

  it('omits the language hint rather than guessing when the locale is unusable', () => {
    expect(transcriptionLanguage('he-IL')).toBe('he');
    expect(transcriptionLanguage('HE')).toBe('he');
    expect(transcriptionLanguage('')).toBeUndefined();
    expect(transcriptionLanguage('klingon')).toBeUndefined();

    const update = buildSessionUpdate({
      agent: { ...agent, language: 'klingon' },
    }) as unknown as SessionAudioInput;
    expect(update.session.audio.input.transcription).toEqual({ model: DEFAULT_TRANSCRIPTION_MODEL });
  });

  it('leaves the audio bridge itself untouched', () => {
    const update = buildSessionUpdate({ agent }) as unknown as {
      session: {
        audio: {
          input: { format: { type: string }; turn_detection: { type: string } };
          output: { format: { type: string } };
        };
      };
    };

    expect(update.session.audio.input.format.type).toBe('audio/pcmu');
    expect(update.session.audio.output.format.type).toBe('audio/pcmu');
    expect(update.session.audio.input.turn_detection.type).toBe('server_vad');
  });
});

describe('conversation logging', () => {
  it('logs completed caller and assistant turns with call identifiers', () => {
    const logger = new RecordingLogger();
    const { twilio, openai } = startBridge({ logger, callId });
    openStream(twilio, openai);

    openai.emit({
      type: 'conversation.item.input_audio_transcription.completed',
      item_id: 'item_user_1',
      transcript: 'שלום, רציתי לבדוק את הסטטוס של ההזמנה',
    });
    openai.emit({
      type: 'response.output_audio_transcript.done',
      item_id: 'item_ai_1',
      transcript: 'בשמחה, מה מספר ההזמנה?',
    });

    expect(logger.messages('[conversation]')).toEqual([
      '[conversation] USER: שלום, רציתי לבדוק את הסטטוס של ההזמנה',
      '[conversation] AI: בשמחה, מה מספר ההזמנה?',
    ]);

    const line = logger.lines.find((entry) => entry.message.startsWith('[conversation] USER'));
    expect(line?.level).toBe('info');
    expect(line?.details).toEqual({ callId, businessId, callSid, streamSid });
  });

  it('accepts the older assistant transcript event name', () => {
    const logger = new RecordingLogger();
    const { twilio, openai } = startBridge({ logger });
    openStream(twilio, openai);

    openai.emit({ type: 'response.audio_transcript.done', transcript: 'תודה ולהתראות' });
    expect(logger.messages('[conversation]')).toEqual(['[conversation] AI: תודה ולהתראות']);
  });

  it('omits the call id when the call row is not known yet', () => {
    const logger = new RecordingLogger();
    const { twilio, openai } = startBridge({ logger });
    openStream(twilio, openai);

    openai.emit({ type: 'conversation.item.input_audio_transcription.completed', transcript: 'הלו' });
    const line = logger.lines.find((entry) => entry.message.startsWith('[conversation]'));
    expect(line?.details['callId']).toBeUndefined();
    expect(line?.details['callSid']).toBe(callSid);
  });

  it('never logs audio payloads or credentials', () => {
    const logger = new RecordingLogger();
    const { twilio, openai } = startBridge({ logger, callId });
    openStream(twilio, openai);

    twilio.emit({ event: 'media', media: { timestamp: '20', payload: 'c2VjcmV0LWF1ZGlv' } });
    openai.emit({ type: 'response.output_audio.delta', delta: 'YXNzaXN0YW50LWF1ZGlv', item_id: 'i1' });
    openai.emit({
      type: 'conversation.item.input_audio_transcription.completed',
      transcript: 'בדיקה',
    });

    const dump = JSON.stringify(logger.lines);
    expect(dump).not.toContain('c2VjcmV0LWF1ZGlv');
    expect(dump).not.toContain('YXNzaXN0YW50LWF1ZGlv');
    expect(dump).not.toMatch(/authorization|bearer|api[_-]?key|sk-/i);
  });

  it('skips empty transcripts and collapses long ones', () => {
    const logger = new RecordingLogger();
    const { twilio, openai } = startBridge({ logger });
    openStream(twilio, openai);

    openai.emit({ type: 'conversation.item.input_audio_transcription.completed', transcript: '   ' });
    openai.emit({ type: 'response.output_audio_transcript.done' });
    expect(logger.messages('[conversation]')).toEqual([]);

    openai.emit({
      type: 'conversation.item.input_audio_transcription.completed',
      transcript: `${'א'.repeat(MAX_LOGGED_TRANSCRIPT_CHARS + 50)}`,
    });
    const logged = logger.messages('[conversation]')[0] ?? '';
    expect(logged.endsWith('…')).toBe(true);
    expect(logged.length).toBeLessThan(MAX_LOGGED_TRANSCRIPT_CHARS + 40);

    expect(truncateTranscript('  multi\n  line   text  ')).toBe('multi line text');
  });

  it('warns without failing the call when transcription errors', () => {
    const logger = new RecordingLogger();
    const { twilio, openai } = startBridge({ logger });
    openStream(twilio, openai);

    openai.emit({ type: 'conversation.item.input_audio_transcription.failed', error: { code: 'x' } });

    expect(logger.lines.some((line) => line.level === 'warn')).toBe(true);
    expect(twilio.closed).toBe(false);
    expect(openai.closed).toBe(false);
  });

  it('logs the composed instructions once, at debug level only', () => {
    const logger = new RecordingLogger();
    const { twilio, openai } = startBridge({ logger });

    const dumps = logger.lines.filter((line) => line.message === 'Composed realtime agent instructions');
    expect(dumps).toHaveLength(1);
    expect(dumps[0]?.level).toBe('debug');
    expect(String(dumps[0]?.details['instructions'])).toMatch(/not a general-purpose assistant/i);
    expect(dumps[0]?.details['callSid']).toBe(callSid);

    // Session churn must not re-dump the policy.
    openStream(twilio, openai);
    openai.emit({ type: 'session.updated', session: { id: 'sess_123' } });
    expect(
      logger.lines.filter((line) => line.message === 'Composed realtime agent instructions'),
    ).toHaveLength(1);
  });
});
