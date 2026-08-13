import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, describe, expect, it } from 'vitest';
import {
  ALLOWLIST_FILENAME,
  createCallerAllowlist,
  loadCallerAllowlist,
  normalizeE164,
  parseAllowListEnv,
} from '../src/dev/caller-allowlist.js';

const silentLogger = { info: (): void => {}, warn: (): void => {} };
const temporaryRoots: string[] = [];

afterEach(async () => {
  await Promise.all(temporaryRoots.splice(0).map((dir) => rm(dir, { recursive: true, force: true })));
});

async function projectRootWith(contents: string | null): Promise<string> {
  const root = await mkdtemp(join(tmpdir(), 'callora-allowlist-'));
  temporaryRoots.push(root);
  if (contents !== null) {
    await writeFile(join(root, ALLOWLIST_FILENAME), contents, 'utf8');
  }
  return root;
}

describe('E.164 normalization', () => {
  it('accepts the formats people paste, and rejects everything else', () => {
    expect(normalizeE164('+972501234567')).toBe('+972501234567');
    expect(normalizeE164('+972 50-123-4567')).toBe('+972501234567');
    expect(normalizeE164('+1 (415) 555-2671')).toBe('+14155552671');

    // No country code, wrong prefix, or not a string at all: never guessed at.
    expect(normalizeE164('0501234567')).toBeNull();
    expect(normalizeE164('+0501234567')).toBeNull();
    expect(normalizeE164('not a number')).toBeNull();
    expect(normalizeE164(undefined)).toBeNull();
    expect(normalizeE164(972501234567)).toBeNull();
  });
});

describe('caller allowlist matching', () => {
  it('allows listed callers and rejects everyone else', () => {
    const { allowlist } = createCallerAllowlist(['+972501234567', '+1 (415) 555-2671']);

    expect(allowlist.enabled).toBe(true);
    expect(allowlist.allows('+972501234567')).toBe(true);
    // Formatting differences on either side must not change the decision.
    expect(allowlist.allows('+972 50 123 4567')).toBe(true);
    expect(allowlist.allows('+14155552671')).toBe(true);

    expect(allowlist.allows('+972509999999')).toBe(false);
    expect(allowlist.allows(undefined)).toBe(false);
    expect(allowlist.allows('anonymous')).toBe(false);
  });

  it('ignores malformed entries but keeps the valid ones', () => {
    const { allowlist, invalid } = createCallerAllowlist(['+972501234567', '0501234567', '']);

    expect(invalid).toEqual(['0501234567', '']);
    expect(allowlist.allows('+972501234567')).toBe(true);
    expect(allowlist.allows('+972509999999')).toBe(false);
  });

  it('treats an empty or all-invalid list as not configured', () => {
    // Copying the example file must not silently reject every caller.
    expect(createCallerAllowlist([]).allowlist.enabled).toBe(false);
    expect(createCallerAllowlist([]).allowlist.allows('+972509999999')).toBe(true);
    expect(createCallerAllowlist(['nonsense']).allowlist.enabled).toBe(false);
  });
});

describe('ALLOW_LIST environment variable', () => {
  it('parses the separators people actually use', () => {
    expect(parseAllowListEnv('+972501234567,+972509998888')).toEqual([
      '+972501234567',
      '+972509998888',
    ]);
    expect(parseAllowListEnv(' +972501234567 ; +972509998888 ')).toEqual([
      '+972501234567',
      '+972509998888',
    ]);
    expect(parseAllowListEnv('+972501234567\n+972509998888')).toEqual([
      '+972501234567',
      '+972509998888',
    ]);
    // Spaces inside a number are part of the number, not a separator.
    expect(parseAllowListEnv('+972 50-123-4567, +972 50-999-8888')).toEqual([
      '+972 50-123-4567',
      '+972 50-999-8888',
    ]);
    // A pasted array literal or quoted value still yields plain numbers.
    expect(parseAllowListEnv('["+972501234567", "+972509998888"]')).toEqual([
      '+972501234567',
      '+972509998888',
    ]);
    expect(parseAllowListEnv('')).toEqual([]);
  });

  it('gates callers from the environment', async () => {
    const root = await projectRootWith(null);
    const allowlist = await loadCallerAllowlist(root, silentLogger, {
      ALLOW_LIST: '+972501234567, +972 50-999-8888',
    });

    expect(allowlist.enabled).toBe(true);
    expect(allowlist.allows('+972501234567')).toBe(true);
    expect(allowlist.allows('+972509998888')).toBe(true);
    expect(allowlist.allows('+972501111111')).toBe(false);
  });

  it('takes precedence over the local file', async () => {
    const root = await projectRootWith('export const allow_list = ["+972501234567"];\n');
    const allowlist = await loadCallerAllowlist(root, silentLogger, { ALLOW_LIST: '+972509998888' });

    expect(allowlist.allows('+972509998888')).toBe(true);
    expect(allowlist.allows('+972501234567')).toBe(false);
  });

  it('falls back to the file when the variable is unset or blank', async () => {
    const root = await projectRootWith('export const allow_list = ["+972501234567"];\n');

    for (const env of [{}, { ALLOW_LIST: '' }, { ALLOW_LIST: '   ' }]) {
      const allowlist = await loadCallerAllowlist(root, silentLogger, env);
      expect(allowlist.allows('+972501234567')).toBe(true);
      expect(allowlist.allows('+972509998888')).toBe(false);
    }
  });

  it('allows every caller when the variable holds nothing usable', async () => {
    const root = await projectRootWith(null);
    const allowlist = await loadCallerAllowlist(root, silentLogger, { ALLOW_LIST: '0501234567' });

    expect(allowlist.enabled).toBe(false);
    expect(allowlist.allows('+972509999999')).toBe(true);
  });
});

describe('loading the local allowlist file', () => {
  it('loads numbers from allowlist.local.js at the project root', async () => {
    const root = await projectRootWith('export const allow_list = ["+972501234567"];\n');
    const allowlist = await loadCallerAllowlist(root, silentLogger, {});

    expect(allowlist.enabled).toBe(true);
    expect(allowlist.allows('+972501234567')).toBe(true);
    expect(allowlist.allows('+972509999999')).toBe(false);
  });

  it('allows every caller when the file is absent, which is the production case', async () => {
    const root = await projectRootWith(null);
    const allowlist = await loadCallerAllowlist(root, silentLogger, {});

    expect(allowlist.enabled).toBe(false);
    expect(allowlist.allows('+972509999999')).toBe(true);
    expect(allowlist.allows(undefined)).toBe(true);
  });

  it('degrades to allowing callers when the file is broken or malformed', async () => {
    const broken = await loadCallerAllowlist(
      await projectRootWith('this is not valid javascript ((('),
      silentLogger,
      {},
    );
    expect(broken.enabled).toBe(false);
    expect(broken.allows('+972509999999')).toBe(true);

    const wrongShape = await loadCallerAllowlist(
      await projectRootWith('export const allow_list = "+972501234567";\n'),
      silentLogger,
    );
    expect(wrongShape.enabled).toBe(false);
    expect(wrongShape.allows('+972509999999')).toBe(true);
  });

  it('matches the shape documented in allowlist.example.js', async () => {
    const example = (await import('../allowlist.example.js')) as { allow_list: unknown };

    expect(Array.isArray(example.allow_list)).toBe(true);
    expect(example.allow_list).toEqual([]);
  });
});
