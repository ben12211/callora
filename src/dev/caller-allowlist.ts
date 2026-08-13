import { pathToFileURL } from 'node:url';
import { e164Schema } from '../http/schemas.js';

/**
 * Caller allowlist, for testing against a real Twilio number without letting unknown
 * callers reach the agent.
 *
 * Two sources, in order: the `ALLOW_LIST` environment variable, which works on a
 * deployed server, and otherwise the gitignored `allowlist.local.js` at the project
 * root, whose shape is documented by `allowlist.example.js`.
 *
 * The allowlist is off unless a source is present and yields at least one valid number,
 * so an unset variable and an undeployed file both behave exactly as before. `From` is
 * used here and nowhere else: business routing still keys on Twilio's `To` only.
 */

export const ALLOWLIST_FILENAME = 'allowlist.local.js';
export const ALLOWLIST_ENV_VAR = 'ALLOW_LIST';

export interface CallerAllowlist {
  enabled: boolean;
  allows(fromNumber: string | undefined): boolean;
}

const ALLOW_ALL: CallerAllowlist = { enabled: false, allows: () => true };

/**
 * Accepts the shapes people actually paste from a phone or the Twilio Console —
 * `+972 50-123-4567`, `+972 (50) 1234567` — and rejects anything that is not E.164
 * once the separators are gone. Never invents a country code.
 */
export function normalizeE164(value: unknown): string | null {
  if (typeof value !== 'string') {
    return null;
  }
  const compact = value.replace(/[\s\-().]/g, '');
  return e164Schema.safeParse(compact).success ? compact : null;
}

interface AllowlistModule {
  allow_list?: unknown;
}

export interface AllowlistLoadResult {
  allowlist: CallerAllowlist;
  /** Entries that were not valid E.164 and are therefore ignored. */
  invalid: string[];
}

export function createCallerAllowlist(entries: readonly unknown[]): AllowlistLoadResult {
  const numbers = new Set<string>();
  const invalid: string[] = [];

  for (const entry of entries) {
    const normalized = normalizeE164(entry);
    if (normalized) {
      numbers.add(normalized);
    } else {
      invalid.push(typeof entry === 'string' ? entry : String(entry));
    }
  }

  if (numbers.size === 0) {
    // An empty list means "not configured" rather than "block everyone", so copying the
    // example file cannot silently reject every call.
    return { allowlist: ALLOW_ALL, invalid };
  }

  return {
    allowlist: {
      enabled: true,
      allows: (fromNumber) => {
        const normalized = normalizeE164(fromNumber);
        return normalized !== null && numbers.has(normalized);
      },
    },
    invalid,
  };
}

/**
 * Splits an `ALLOW_LIST` value into candidate numbers. Commas, semicolons, and newlines
 * separate entries — but not spaces, so a number written `+972 50-123-4567` survives as
 * one entry. Surrounding brackets and quotes are tolerated so a pasted array works too.
 */
export function parseAllowListEnv(value: string): string[] {
  return value
    .replace(/^\s*\[|\]\s*$/g, '')
    .split(/[,;\r\n]+/)
    .map((entry) => entry.trim().replace(/^["']|["']$/g, '').trim())
    .filter((entry) => entry.length > 0);
}

export interface AllowlistLogger {
  info(details: Record<string, unknown>, message: string): void;
  warn(details: Record<string, unknown>, message: string): void;
}

/**
 * Resolves the allowlist from `ALLOW_LIST` if it is set, otherwise from the gitignored
 * local file. The environment wins so a server can be gated without shipping a file,
 * and any absence, emptiness, or error leaves every caller allowed.
 */
export async function loadCallerAllowlist(
  projectRoot: string,
  logger: AllowlistLogger,
  env: NodeJS.ProcessEnv = process.env,
): Promise<CallerAllowlist> {
  const fromEnv = env[ALLOWLIST_ENV_VAR]?.trim();
  if (fromEnv) {
    const { allowlist, invalid } = createCallerAllowlist(parseAllowListEnv(fromEnv));
    if (invalid.length > 0) {
      logger.warn(
        { source: ALLOWLIST_ENV_VAR, invalid },
        'Ignoring allowlist entries that are not E.164',
      );
    }
    logger.info(
      { source: ALLOWLIST_ENV_VAR, enabled: allowlist.enabled },
      allowlist.enabled
        ? 'Caller allowlist active from the environment; other callers will be rejected'
        : 'ALLOW_LIST contained no usable numbers; all callers allowed',
    );
    return allowlist;
  }

  const fileUrl = new URL(ALLOWLIST_FILENAME, pathToFileURL(`${projectRoot}/`));

  let loaded: AllowlistModule;
  try {
    loaded = (await import(fileUrl.href)) as AllowlistModule;
  } catch {
    logger.info({ source: ALLOWLIST_FILENAME }, 'No caller allowlist configured; all callers allowed');
    return ALLOW_ALL;
  }

  if (!Array.isArray(loaded.allow_list)) {
    logger.warn(
      { source: ALLOWLIST_FILENAME },
      'Caller allowlist file does not export an allow_list array; all callers allowed',
    );
    return ALLOW_ALL;
  }

  const { allowlist, invalid } = createCallerAllowlist(loaded.allow_list);
  if (invalid.length > 0) {
    logger.warn({ source: ALLOWLIST_FILENAME, invalid }, 'Ignoring allowlist entries that are not E.164');
  }
  logger.info(
    { source: ALLOWLIST_FILENAME, enabled: allowlist.enabled },
    allowlist.enabled
      ? 'Caller allowlist active from the local file; other callers will be rejected'
      : 'Caller allowlist file is empty; all callers allowed',
  );
  return allowlist;
}
