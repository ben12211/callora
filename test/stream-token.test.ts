import { describe, expect, it } from 'vitest';
import { createStreamToken, verifyStreamToken } from '../src/http/stream-token.js';

/**
 * The Media Stream token used to be signed with TWILIO_AUTH_TOKEN, so one credential did
 * two unrelated jobs. `STREAM_TOKEN_SECRET` separates them, and verification accepts a
 * list so introducing or rotating it does not reject the tokens already in flight — a
 * token lives five minutes, and a call that started before the change still has to
 * connect.
 */

const claims = { callSid: 'CA1', businessId: '00000000-0000-4000-8000-000000000001' };

describe('stream token secrets', () => {
  it('accepts a token signed with any configured secret', () => {
    const previous = createStreamToken('the-old-twilio-auth-token', claims);
    const current = createStreamToken('the-new-dedicated-secret', claims);
    const secrets = ['the-new-dedicated-secret', 'the-old-twilio-auth-token'];

    expect(verifyStreamToken(secrets, current)).toMatchObject(claims);
    expect(verifyStreamToken(secrets, previous)).toMatchObject(claims);
  });

  it('rejects a token signed with a secret that is no longer configured', () => {
    const retired = createStreamToken('a-retired-secret', claims);
    expect(verifyStreamToken(['the-new-dedicated-secret'], retired)).toBeNull();
  });

  it('still takes a single secret, as the webhook and the tests did before', () => {
    const token = createStreamToken('one-secret', claims);
    expect(verifyStreamToken('one-secret', token)).toMatchObject(claims);
    expect(verifyStreamToken('another-secret', token)).toBeNull();
  });

  it('rejects an expired token whichever secret signed it', () => {
    const token = createStreamToken('one-secret', claims, 300, 0);
    expect(verifyStreamToken(['one-secret'], token, 301_000)).toBeNull();
    expect(verifyStreamToken(['one-secret'], token, 299_000)).toMatchObject(claims);
  });
});
