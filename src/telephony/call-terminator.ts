import twilio from 'twilio';
import type { AppConfig } from '../config.js';

/**
 * Hangs up live Twilio calls by CallSid.
 *
 * The CallSid always comes from the server-side stream authorization, never from the
 * model or the caller. Termination is idempotent: a call already completed by Twilio
 * (the caller hung up first, or a retry arrived) is a success, not an error.
 */
export interface CallTerminator {
  endCall(callSid: string): Promise<void>;
}

/** Twilio REST errors carry a numeric `status` and `code`. */
function twilioErrorStatus(error: unknown): { status?: number; code?: number } {
  if (typeof error !== 'object' || error === null) {
    return {};
  }
  const { status, code } = error as { status?: unknown; code?: unknown };
  return {
    status: typeof status === 'number' ? status : undefined,
    code: typeof code === 'number' ? code : undefined,
  };
}

/**
 * 404 means the call no longer exists and 20009/21220 mean it is not in a state that can
 * be modified — in every case the call is already over, which is the desired outcome.
 */
function alreadyEnded(error: unknown): boolean {
  const { status, code } = twilioErrorStatus(error);
  return status === 404 || code === 20404 || code === 20009 || code === 21220;
}

export function createCallTerminator(config: AppConfig): CallTerminator {
  const client = twilio(config.twilioAccountSid, config.twilioAuthToken);
  const terminated = new Set<string>();

  return {
    async endCall(callSid: string): Promise<void> {
      if (terminated.has(callSid)) {
        return;
      }
      // Recorded before the request so a concurrent second attempt cannot double-send.
      terminated.add(callSid);

      try {
        await client.calls(callSid).update({ status: 'completed' });
      } catch (error) {
        if (alreadyEnded(error)) {
          return;
        }
        terminated.delete(callSid);
        throw error;
      }
    },
  };
}
