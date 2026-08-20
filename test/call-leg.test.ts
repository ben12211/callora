import { describe, expect, it, vi } from 'vitest';
import { CartesiaBridge } from '../src/realtime/cartesia-bridge.js';
import { HangupSequence, SilenceWatchdog } from '../src/realtime/call-leg.js';
import { ElevenLabsBridge } from '../src/realtime/elevenlabs-bridge.js';
import { stillThereLine } from '../src/realtime/policy.js';
import {
  FakeChannel,
  agent,
  businessId,
  callSid,
  flush,
  silentLogger,
  streamSid,
} from './support/realtime-harness.js';

/**
 * These cover the pieces every provider bridge now shares. Silence escalation used to
 * exist only on the OpenAI bridge, which left an ElevenLabs or Cartesia call with a
 * silent caller holding a Twilio leg and a provider session open until the hour-long
 * ceiling.
 */

function fakeCartesiaSocket() {
  const sentText: Record<string, unknown>[] = [];
  const sentBinary: Buffer[] = [];
  let onMessage: ((raw: string) => void) | null = null;
  return {
    socket: {
      sendText: (payload: string) => sentText.push(JSON.parse(payload) as Record<string, unknown>),
      sendBinary: (payload: Buffer) => sentBinary.push(payload),
      close: () => {},
      onMessage: (handler: (raw: string) => void) => {
        onMessage = handler;
      },
      onClose: () => {},
      onError: () => {},
    },
    sentText,
    sentBinary,
    emit: (message: Record<string, unknown>) => onMessage?.(JSON.stringify(message)),
  };
}

describe('HangupSequence', () => {
  const base = {
    businessId,
    callSid,
    logger: silentLogger,
  };

  it('waits for the audio to drain before terminating', async () => {
    let drained = false;
    const endCall = vi.fn(async () => {});
    const finished = vi.fn();
    const hangup = new HangupSequence({
      ...base,
      endCall,
      audioDrained: () => drained,
      onFinished: finished,
    });

    hangup.beginDraining('model_requested');
    expect(endCall).not.toHaveBeenCalled();
    expect(hangup.current).toBe('draining');

    drained = true;
    hangup.terminateWhenDrained();
    await flush();

    expect(endCall).toHaveBeenCalledExactlyOnceWith('model_requested');
    expect(finished).toHaveBeenCalledExactlyOnceWith('model_requested');
    expect(hangup.current).toBe('terminated');
  });

  it('terminates exactly once however many times it is asked', async () => {
    const endCall = vi.fn(async () => {});
    const hangup = new HangupSequence({
      ...base,
      endCall,
      audioDrained: () => true,
      onFinished: () => {},
    });

    hangup.beginDraining('first');
    hangup.terminate('second');
    hangup.terminate('third');
    await flush();

    expect(endCall).toHaveBeenCalledExactlyOnceWith('first');
  });

  it('still closes the bridge when the Twilio hangup fails', async () => {
    const finished = vi.fn();
    const hangup = new HangupSequence({
      ...base,
      endCall: async () => {
        throw new Error('twilio unavailable');
      },
      audioDrained: () => true,
      onFinished: finished,
    });

    hangup.beginDraining('model_requested');
    await flush();

    expect(finished).toHaveBeenCalledExactlyOnceWith('model_requested');
  });

  it('hangs up anyway when the goodbye never drains', async () => {
    vi.useFakeTimers();
    try {
      const finished = vi.fn();
      const hangup = new HangupSequence({
        ...base,
        audioDrained: () => false,
        onFinished: finished,
        drainTimeoutMs: 5_000,
      });

      hangup.beginDraining('model_requested');
      expect(finished).not.toHaveBeenCalled();

      vi.advanceTimersByTime(5_000);
      expect(finished).toHaveBeenCalledExactlyOnceWith('farewell-drain-timeout');
    } finally {
      vi.useRealTimers();
    }
  });
});

describe('SilenceWatchdog', () => {
  it('escalates from a check-in to a hangup, and resets when the caller speaks', () => {
    vi.useFakeTimers();
    try {
      const onPrompt = vi.fn();
      const onHangup = vi.fn();
      const watchdog = new SilenceWatchdog({
        promptAfterMs: 1_000,
        hangupAfterMs: 1_000,
        armed: () => true,
        agentSpeaking: () => false,
        onPrompt,
        onHangup,
      });

      watchdog.restart();
      vi.advanceTimersByTime(1_000);
      expect(onPrompt).toHaveBeenCalledOnce();
      expect(onHangup).not.toHaveBeenCalled();

      // The caller answered, so the escalation starts over rather than hanging up.
      watchdog.reset();
      vi.advanceTimersByTime(1_000);
      expect(onPrompt).toHaveBeenCalledTimes(2);
      expect(onHangup).not.toHaveBeenCalled();

      vi.advanceTimersByTime(1_000);
      expect(onHangup).toHaveBeenCalledOnce();
    } finally {
      vi.useRealTimers();
    }
  });

  it('does not count the agent speaking as caller silence', () => {
    vi.useFakeTimers();
    try {
      let speaking = true;
      const onPrompt = vi.fn();
      const watchdog = new SilenceWatchdog({
        promptAfterMs: 1_000,
        hangupAfterMs: 1_000,
        armed: () => true,
        agentSpeaking: () => speaking,
        onPrompt,
        onHangup: () => {},
      });

      watchdog.restart();
      vi.advanceTimersByTime(1_000);
      expect(onPrompt).not.toHaveBeenCalled();

      speaking = false;
      vi.advanceTimersByTime(1_000);
      expect(onPrompt).toHaveBeenCalledOnce();
    } finally {
      vi.useRealTimers();
    }
  });

  it('runs one stage when the provider gives no way to speak unprompted', () => {
    vi.useFakeTimers();
    try {
      const onHangup = vi.fn();
      const watchdog = new SilenceWatchdog({
        promptAfterMs: 1_000,
        hangupAfterMs: 1_000,
        armed: () => true,
        agentSpeaking: () => false,
        onHangup,
      });

      watchdog.restart();
      vi.advanceTimersByTime(1_000);
      expect(onHangup).not.toHaveBeenCalled();

      // Without a prompt stage the whole window is spent before hanging up.
      vi.advanceTimersByTime(1_000);
      expect(onHangup).toHaveBeenCalledOnce();
    } finally {
      vi.useRealTimers();
    }
  });

  it('stops once it is disarmed', () => {
    vi.useFakeTimers();
    try {
      const onHangup = vi.fn();
      let armed = true;
      const watchdog = new SilenceWatchdog({
        promptAfterMs: 1_000,
        hangupAfterMs: 1_000,
        armed: () => armed,
        agentSpeaking: () => false,
        onHangup,
      });

      watchdog.restart();
      armed = false;
      vi.advanceTimersByTime(5_000);
      expect(onHangup).not.toHaveBeenCalled();
    } finally {
      vi.useRealTimers();
    }
  });
});

describe('silence handling reaches every provider', () => {
  it('ends an ElevenLabs call whose caller has gone silent', async () => {
    vi.useFakeTimers();
    const endCall = vi.fn(async () => {});
    try {
      const twilio = new FakeChannel();
      const elevenlabs = new FakeChannel();
      const bridge = new ElevenLabsBridge({
        twilio,
        elevenlabs,
        agent: { ...agent, voiceProvider: 'elevenlabs' },
        businessId,
        callSid,
        logger: silentLogger,
        endCall,
        silence: { promptAfterMs: 1_000, hangupAfterMs: 1_000 },
      });
      bridge.start();
      twilio.emit({ event: 'start', streamSid, start: { streamSid, callSid, accountSid: 'AC1' } });

      vi.advanceTimersByTime(2_000);
      await vi.runAllTicks();
      // Nothing is queued for the caller, so the drain completes immediately.
      expect(endCall).toHaveBeenCalledExactlyOnceWith('caller_silent');
    } finally {
      vi.useRealTimers();
    }
  });

  it('keeps an ElevenLabs call alive while the caller is still talking', () => {
    vi.useFakeTimers();
    const endCall = vi.fn(async () => {});
    try {
      const twilio = new FakeChannel();
      const elevenlabs = new FakeChannel();
      const bridge = new ElevenLabsBridge({
        twilio,
        elevenlabs,
        agent: { ...agent, voiceProvider: 'elevenlabs' },
        businessId,
        callSid,
        logger: silentLogger,
        endCall,
        silence: { promptAfterMs: 1_000, hangupAfterMs: 1_000 },
      });
      bridge.start();
      twilio.emit({ event: 'start', streamSid, start: { streamSid, callSid, accountSid: 'AC1' } });

      vi.advanceTimersByTime(1_500);
      elevenlabs.emit({
        type: 'user_transcript',
        user_transcription_event: { user_transcript: 'I am still here' },
      });
      vi.advanceTimersByTime(1_500);

      expect(endCall).not.toHaveBeenCalled();
    } finally {
      vi.useRealTimers();
    }
  });

  it('asks a silent Cartesia caller whether they are there before hanging up', async () => {
    vi.useFakeTimers();
    const endCall = vi.fn(async () => {});
    try {
      const twilio = new FakeChannel();
      const stt = fakeCartesiaSocket();
      const tts = fakeCartesiaSocket();
      const cartesiaAgent = { ...agent, voiceProvider: 'cartesia' as const, greeting: 'שלום' };
      const bridge = new CartesiaBridge({
        twilio,
        stt: stt.socket,
        tts: tts.socket,
        agent: cartesiaAgent,
        businessId,
        callSid,
        ttsModel: 'sonic-3.5',
        voiceId: 'voice-uuid',
        llm: { baseUrl: 'https://llm.example.test', apiKey: 'sk-test', model: 'gpt-5' },
        logger: silentLogger,
        endCall,
        silence: { promptAfterMs: 1_000, hangupAfterMs: 1_000 },
      });
      bridge.start();
      twilio.emit({ event: 'start', streamSid, start: { streamSid, callSid, accountSid: 'AC1' } });

      // The greeting counts as the agent speaking; only once Sonic has finished it and
      // Twilio has played it does the caller's silence start to matter.
      const greetingContext = String(tts.sentText[0]?.['context_id'] ?? '');
      tts.emit({ type: 'done', context_id: greetingContext });

      vi.advanceTimersByTime(1_000);
      const spoken = tts.sentText.map((message) => String(message['transcript'] ?? ''));
      expect(spoken).toContain(stillThereLine(cartesiaAgent));
      expect(endCall).not.toHaveBeenCalled();

      // The check-in itself finishes playing too, and is still met with silence.
      const promptContext = String(tts.sentText[tts.sentText.length - 1]?.['context_id'] ?? '');
      tts.emit({ type: 'done', context_id: promptContext });

      vi.advanceTimersByTime(1_000);
      await vi.runAllTicks();
      expect(endCall).toHaveBeenCalledExactlyOnceWith('caller_silent');
    } finally {
      vi.useRealTimers();
    }
  });
});

describe('stillThereLine', () => {
  it('speaks the tenant language, and falls back to English', () => {
    expect(stillThereLine({ language: 'he-IL' })).toBe('אתה עדיין איתי?');
    expect(stillThereLine({ language: 'en-US' })).toBe('Are you still there?');
    expect(stillThereLine({ language: 'klingon' })).toBe('Are you still there?');
  });
});
