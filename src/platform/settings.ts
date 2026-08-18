import { createHash } from 'node:crypto';
import type { AppConfig, ProviderCredentials, RealtimeProvider } from '../config.js';
import { resolveProviderSettings } from '../config.js';
import type { DataStore } from '../db/store.js';
import {
  ALLOWLIST_ENV_VAR,
  createCallerAllowlist,
  parseAllowListEnv,
  type CallerAllowlist,
} from '../dev/caller-allowlist.js';
import { REALTIME_PROVIDERS } from '../realtime/provider.js';
import type { SecretBox } from './secret-box.js';

/**
 * Platform settings, managed from the dashboard.
 *
 * Every key is the name of an environment variable this deployment already understood.
 * The environment stays the floor: a saved value overrides it, clearing a saved value
 * falls back to it, and a deployment that never opens the settings page behaves exactly
 * as it did before. That is what makes this safe to add to a running stack — and what
 * lets a fresh one come up with nothing but a database and be configured in the browser.
 *
 * Values are resolved once here and read live by the call path, so a saved credential
 * takes effect on the next call rather than the next restart.
 */

/**
 * How long a resolved snapshot is trusted before the next read re-checks the table.
 *
 * Saving in the dashboard updates the process that handled the request immediately; this
 * is what makes a change reach every *other* reader — a second instance, or a value
 * written straight into the database — without anyone restarting anything.
 */
export const SETTINGS_TTL_MS = 5_000;

/** Background sweep, so pages and status endpoints converge even between calls. */
export const SETTINGS_REFRESH_INTERVAL_MS = 60_000;

export const SETTING_GROUPS = ['platform', ...REALTIME_PROVIDERS] as const;
export type SettingGroup = (typeof SETTING_GROUPS)[number];

export interface SettingDescriptor {
  /** Identical to the environment variable it overrides. */
  key: string;
  label: string;
  group: SettingGroup;
  /** Sealed before storage and never rendered back to a browser. */
  secret: boolean;
  hint: string;
  multiline?: boolean;
  choices?: readonly string[];
}

export const SETTING_CATALOG: readonly SettingDescriptor[] = [
  {
    key: 'VOICE_PROVIDER',
    label: 'Default provider for new agents',
    group: 'platform',
    secret: false,
    hint: 'Each business still picks its own provider; this is only what a newly created agent starts on.',
    choices: REALTIME_PROVIDERS,
  },
  {
    key: ALLOWLIST_ENV_VAR,
    label: 'Caller allowlist',
    group: 'platform',
    secret: false,
    hint: 'E.164 numbers separated by commas or new lines. Only these callers reach an agent; empty allows everyone.',
    multiline: true,
  },
  {
    key: 'OPENAI_API_KEY',
    label: 'API key',
    group: 'openai',
    secret: true,
    hint: 'Used for the Realtime bridge, and for the reasoning turn of the Cartesia pipeline.',
  },
  {
    key: 'OPENAI_REALTIME_URL',
    label: 'Realtime endpoint',
    group: 'openai',
    secret: false,
    hint: 'Override only to point at a proxy or a regional endpoint.',
  },
  {
    key: 'OPENAI_TRANSCRIBE_MODEL',
    label: 'Transcription model',
    group: 'openai',
    secret: false,
    hint: 'Transcribes caller audio for the conversation log. It never drives the reply.',
  },
  {
    key: 'ELEVENLABS_API_KEY',
    label: 'API key',
    group: 'elevenlabs',
    secret: true,
    hint: 'Exchanged server-side for a short-lived signed URL; it never reaches the browser or Twilio.',
  },
  {
    key: 'ELEVENLABS_AGENT_ID',
    label: 'Agent id',
    group: 'elevenlabs',
    secret: false,
    hint: 'The agent must accept ulaw_8000 audio and have the prompt, first message, and language overrides enabled.',
  },
  {
    key: 'ELEVENLABS_API_BASE_URL',
    label: 'API base URL',
    group: 'elevenlabs',
    secret: false,
    hint: 'Override only to point at a proxy or a regional endpoint.',
  },
  {
    key: 'CARTESIA_API_KEY',
    label: 'API key',
    group: 'cartesia',
    secret: true,
    hint: 'Cartesia covers speech only, so this provider also needs the OpenAI key above.',
  },
  {
    key: 'CARTESIA_VOICE_ID',
    label: 'Default voice id',
    group: 'cartesia',
    secret: false,
    hint: 'Sonic voice UUID used by agents that did not choose their own.',
  },
  {
    key: 'CARTESIA_TTS_MODEL',
    label: 'TTS model',
    group: 'cartesia',
    secret: false,
    hint: 'Must support the languages your businesses answer in; Hebrew needs sonic-3 or later.',
  },
  {
    key: 'CARTESIA_STT_MODEL',
    label: 'STT model',
    group: 'cartesia',
    secret: false,
    hint: 'ink-whisper is multilingual; ink-2 is faster but English only.',
  },
  { key: 'CARTESIA_VERSION', label: 'API version', group: 'cartesia', secret: false, hint: 'Cartesia API date version.' },
  {
    key: 'CARTESIA_WS_BASE_URL',
    label: 'WebSocket base URL',
    group: 'cartesia',
    secret: false,
    hint: 'Override only to point at a proxy or a regional endpoint.',
  },
  {
    key: 'TEXT_LLM_MODEL',
    label: 'Reasoning model',
    group: 'cartesia',
    secret: false,
    hint: 'Default for new Cartesia agents; each agent can override it in its own Model field.',
  },
  {
    key: 'TEXT_LLM_BASE_URL',
    label: 'Reasoning API base URL',
    group: 'cartesia',
    secret: false,
    hint: 'OpenAI-compatible chat completions endpoint.',
  },
];

const CATALOG_BY_KEY = new Map(SETTING_CATALOG.map((descriptor) => [descriptor.key, descriptor]));

/** Where the value in force came from, so an operator can tell what a save would change. */
export type SettingSource = 'callora' | 'environment' | 'default';

export interface SettingView extends SettingDescriptor {
  /** The value in force. Always empty for secrets: a stored credential is never rendered. */
  value: string;
  configured: boolean;
  source: SettingSource;
  /** True when a stored secret cannot be decrypted with the current SECRETS_KEY. */
  unreadable: boolean;
}

export class SettingsValidationError extends Error {}

export interface SettingsLogger {
  info(details: Record<string, unknown>, message: string): void;
  warn(details: Record<string, unknown>, message: string): void;
  error(details: Record<string, unknown>, message: string): void;
}

export interface SaveSettingsInput {
  /** Submitted fields, by key. A key that was not submitted is left alone. */
  values: Record<string, string | undefined>;
  /** Secrets to drop, falling back to the environment. */
  clear?: readonly string[];
  /**
   * The revision the form was rendered from. When it no longer matches, the submission is
   * refused: the page was showing values somebody else has since changed, and an
   * untouched field on it would otherwise silently roll their change back.
   */
  expectedRevision?: string;
}

/**
 * The live platform configuration.
 *
 * The call path asks this object rather than `AppConfig` so that saving a credential in
 * the dashboard is picked up by the next call without a restart.
 */
export interface PlatformRuntime {
  /** Re-reads the stored settings when the current snapshot is older than `maxAgeMs`. */
  refreshIfStale(maxAgeMs?: number): Promise<void>;
  /** Fingerprint of the stored settings, so a form can detect a concurrent change. */
  revision(): string;
  providers(): ProviderCredentials;
  defaultProvider(): RealtimeProvider;
  allowlist(): CallerAllowlist;
  /** Merged values, for code that reports which variables a provider is still missing. */
  environment(): NodeJS.ProcessEnv;
  view(): SettingView[];
  /** False when no SECRETS_KEY is configured, so credentials cannot be stored safely. */
  secretsEditable(): boolean;
}

export interface PlatformSettingsOptions {
  store: DataStore;
  /** What the process started with; the floor every saved value is merged over. */
  config: AppConfig;
  secretBox: SecretBox | null;
  logger: SettingsLogger;
  environment?: NodeJS.ProcessEnv;
  /** Startup-loaded allowlist (the gitignored local file), used when no value is set. */
  fallbackAllowlist?: CallerAllowlist;
  /** How long a snapshot is trusted; 0 re-reads on every check. Defaults to `SETTINGS_TTL_MS`. */
  ttlMs?: number;
}

const ALLOW_EVERY_CALLER: CallerAllowlist = { enabled: false, allows: () => true };

/** The fingerprint of a table with nothing in it. */
const EMPTY_REVISION = 'none';

/**
 * Fingerprint of the stored rows. Sealed values are hashed as they are stored, so this
 * never handles a plaintext credential.
 */
function revisionOf(rows: readonly { key: string; value: string }[]): string {
  if (rows.length === 0) {
    return EMPTY_REVISION;
  }
  const hash = createHash('sha256');
  for (const row of [...rows].sort((left, right) => left.key.localeCompare(right.key))) {
    hash.update(row.key).update(' ').update(row.value).update(' ');
  }
  return hash.digest('base64url').slice(0, 22);
}

/**
 * Recovers the settings values the running configuration was built from.
 *
 * Reading them back off `AppConfig` rather than off `process.env` keeps one source of
 * truth: whatever the process is actually running with is the floor, including in tests
 * and in any embedder that builds its configuration by hand.
 */
export function platformValuesFromConfig(
  config: AppConfig,
  environment: NodeJS.ProcessEnv = process.env,
): Record<string, string> {
  const { openai, elevenlabs, cartesia } = config.providers;
  const values: Record<string, string> = { VOICE_PROVIDER: config.voiceProvider };
  const set = (key: string, value: string | undefined): void => {
    if (value) {
      values[key] = value;
    }
  };

  set('OPENAI_API_KEY', openai?.apiKey);
  set('OPENAI_REALTIME_URL', openai?.realtimeUrl);
  set('OPENAI_TRANSCRIBE_MODEL', openai?.transcribeModel);
  set('ELEVENLABS_API_KEY', elevenlabs?.apiKey);
  set('ELEVENLABS_AGENT_ID', elevenlabs?.agentId);
  set('ELEVENLABS_API_BASE_URL', elevenlabs?.apiBaseUrl);
  set('CARTESIA_API_KEY', cartesia?.apiKey);
  set('CARTESIA_VOICE_ID', cartesia?.defaultVoiceId);
  set('CARTESIA_TTS_MODEL', cartesia?.ttsModel);
  set('CARTESIA_STT_MODEL', cartesia?.sttModel);
  set('CARTESIA_VERSION', cartesia?.version);
  set('CARTESIA_WS_BASE_URL', cartesia?.wsBaseUrl);
  set('TEXT_LLM_MODEL', cartesia?.textLlmModel);
  set('TEXT_LLM_BASE_URL', cartesia?.textLlmBaseUrl);
  // The allowlist is not part of AppConfig; it is resolved at startup from this variable
  // or from the gitignored local file, which stays available as the fallback.
  set(ALLOWLIST_ENV_VAR, environment[ALLOWLIST_ENV_VAR]);

  return values;
}

export class PlatformSettings implements PlatformRuntime {
  private readonly base: Record<string, string>;
  private overrides = new Map<string, string>();
  private unreadable = new Set<string>();

  /** When the stored settings were last read, so a reader can tell a stale snapshot. */
  private lastLoadedAt = 0;
  /** The read in progress, so a burst of calls does not become a burst of queries. */
  private inFlight: Promise<void> | null = null;
  /** Fingerprint of the rows behind the current snapshot. */
  private storedRevision = EMPTY_REVISION;

  private merged: Record<string, string>;
  private resolvedProviders: ProviderCredentials;
  private resolvedDefault: RealtimeProvider;
  private resolvedAllowlist: CallerAllowlist;

  public constructor(private readonly options: PlatformSettingsOptions) {
    this.base = platformValuesFromConfig(options.config, options.environment);
    this.merged = { ...this.base };
    this.resolvedProviders = options.config.providers;
    this.resolvedDefault = options.config.voiceProvider;
    this.resolvedAllowlist = options.fallbackAllowlist ?? ALLOW_EVERY_CALLER;
    this.recompute();
  }

  /**
   * Reads the stored overrides. A failure here — an unmigrated database, an unreachable
   * one — leaves the deployment running on its environment rather than refusing to start.
   */
  public async load(): Promise<void> {
    try {
      const rows = await this.options.store.listPlatformSettings();
      this.storedRevision = revisionOf(rows);
      const overrides = new Map<string, string>();
      const unreadable = new Set<string>();

      for (const row of rows) {
        // A key this build no longer knows is ignored rather than fed to the schema.
        if (!CATALOG_BY_KEY.has(row.key)) {
          continue;
        }
        if (!row.secret) {
          overrides.set(row.key, row.value);
          continue;
        }
        if (!this.options.secretBox) {
          unreadable.add(row.key);
          continue;
        }
        try {
          overrides.set(row.key, this.options.secretBox.open(row.value));
        } catch {
          unreadable.add(row.key);
        }
      }

      this.overrides = overrides;
      this.unreadable = unreadable;
      if (unreadable.size > 0) {
        this.options.logger.error(
          { keys: [...unreadable] },
          'Stored platform secrets could not be decrypted with this SECRETS_KEY; using the environment instead',
        );
      }
    } catch (error) {
      this.options.logger.warn(
        { error: error instanceof Error ? error.message : 'unknown error' },
        'Could not read platform settings; continuing with the environment',
      );
    }
    // Recorded even when the read failed: a database that is down must not be retried on
    // every single call.
    this.lastLoadedAt = Date.now();
    this.recompute();
  }

  /**
   * Re-reads the settings if this snapshot has aged out.
   *
   * Called on the paths where being out of date is visible — answering a call, opening a
   * bridge, rendering the settings — so a credential saved on one instance is in force
   * everywhere within seconds rather than at the next deployment.
   */
  public async refreshIfStale(maxAgeMs = this.options.ttlMs ?? SETTINGS_TTL_MS): Promise<void> {
    if (Date.now() - this.lastLoadedAt < maxAgeMs) {
      return;
    }
    this.inFlight ??= this.load().finally(() => {
      this.inFlight = null;
    });
    await this.inFlight;
  }

  public revision(): string {
    return this.storedRevision;
  }

  public providers(): ProviderCredentials {
    return this.resolvedProviders;
  }

  public defaultProvider(): RealtimeProvider {
    return this.resolvedDefault;
  }

  public allowlist(): CallerAllowlist {
    return this.resolvedAllowlist;
  }

  public environment(): NodeJS.ProcessEnv {
    return this.merged;
  }

  public secretsEditable(): boolean {
    return this.options.secretBox !== null;
  }

  public view(): SettingView[] {
    return SETTING_CATALOG.map((descriptor) => {
      const effective = this.merged[descriptor.key] ?? '';
      const overridden = this.overrides.has(descriptor.key);
      const source: SettingSource = overridden
        ? 'callora'
        : this.base[descriptor.key]
          ? 'environment'
          : 'default';
      return {
        ...descriptor,
        // A stored credential is never rendered, not even masked: the page only reports
        // that one exists and where it came from.
        value: descriptor.secret ? '' : effective,
        configured: effective.trim() !== '',
        source,
        unreadable: this.unreadable.has(descriptor.key),
      };
    });
  }

  /**
   * Writes the submitted settings and re-resolves the live configuration.
   *
   * A blank field means "fall back to the environment", so clearing a value in the form is
   * how an override is removed. Blank secrets are the exception: an empty password field
   * means the operator did not retype the credential, so the stored one is kept and only
   * an explicit `clear` removes it.
   */
  public async save(input: SaveSettingsInput): Promise<string[]> {
    if (input.expectedRevision !== undefined && input.expectedRevision !== this.storedRevision) {
      throw new SettingsValidationError(
        'These settings were changed somewhere else while this page was open. Reload it and make the change again.',
      );
    }

    // Everything is validated before anything is written: a rejected field must not leave
    // half of a provider's configuration stored and the other half not.
    const planned = this.plan(input);

    for (const step of planned) {
      if (step.remove) {
        await this.options.store.deletePlatformSetting(step.key);
      } else {
        await this.options.store.upsertPlatformSetting({
          key: step.key,
          value: step.value,
          secret: step.secret,
        });
      }
    }

    if (planned.length > 0) {
      await this.load();
    }
    return planned.map((step) => step.key);
  }

  private plan(input: SaveSettingsInput): { key: string; value: string; secret: boolean; remove: boolean }[] {
    const clear = new Set(input.clear ?? []);
    const steps: { key: string; value: string; secret: boolean; remove: boolean }[] = [];

    for (const descriptor of SETTING_CATALOG) {
      const { key, secret } = descriptor;

      if (clear.has(key)) {
        if (this.overrides.has(key) || this.unreadable.has(key)) {
          steps.push({ key, value: '', secret, remove: true });
        }
        continue;
      }

      const submitted = input.values[key];
      if (submitted === undefined) {
        continue;
      }
      const value = submitted.trim();

      if (value === '') {
        // Nothing typed into a secret field means "leave the stored credential alone".
        if (secret) {
          continue;
        }
        if (this.overrides.has(key)) {
          steps.push({ key, value: '', secret, remove: true });
        }
        continue;
      }

      // The form is pre-filled with the values in force, so an untouched field must not
      // turn into a stored override of an environment variable.
      if (secret ? this.overrides.get(key) === value : value === (this.merged[key] ?? '')) {
        continue;
      }

      this.validate(descriptor, value);
      if (secret && !this.options.secretBox) {
        throw new SettingsValidationError(
          `${key} cannot be stored because this deployment has no SECRETS_KEY; set one, or keep the credential in the environment.`,
        );
      }

      steps.push({
        key,
        value: secret ? this.options.secretBox!.seal(value) : value,
        secret,
        remove: false,
      });
    }

    return steps;
  }

  private validate(descriptor: SettingDescriptor, value: string): void {
    if (descriptor.choices && !descriptor.choices.includes(value)) {
      throw new SettingsValidationError(
        `${descriptor.key} must be one of: ${descriptor.choices.join(', ')}.`,
      );
    }
    if (descriptor.key === ALLOWLIST_ENV_VAR) {
      const { allowlist, invalid } = createCallerAllowlist(parseAllowListEnv(value));
      if (invalid.length > 0) {
        throw new SettingsValidationError(
          `These allowlist entries are not valid E.164 numbers: ${invalid.join(', ')}.`,
        );
      }
      if (!allowlist.enabled) {
        throw new SettingsValidationError('The allowlist must contain at least one E.164 number, or be left empty.');
      }
    }
  }

  private recompute(): void {
    const merged: Record<string, string> = { ...this.base };
    for (const [key, value] of this.overrides) {
      merged[key] = value;
    }
    // A blank value is an unset one, exactly as an empty environment variable is.
    for (const [key, value] of Object.entries(merged)) {
      if (value.trim() === '') {
        delete merged[key];
      }
    }

    try {
      const resolved = resolveProviderSettings(merged);
      this.merged = merged;
      this.resolvedProviders = resolved.providers;
      this.resolvedDefault = resolved.voiceProvider;
    } catch (error) {
      // Only reachable if a row was written straight into the database: `save` validates.
      // The previous snapshot keeps serving calls rather than a half-resolved one.
      this.options.logger.error(
        { error: error instanceof Error ? error.message : 'unknown error' },
        'Stored platform settings are not usable; keeping the previous configuration',
      );
      return;
    }

    this.resolvedAllowlist = this.buildAllowlist(merged[ALLOWLIST_ENV_VAR]);
  }

  private buildAllowlist(raw: string | undefined): CallerAllowlist {
    if (!raw?.trim()) {
      return this.options.fallbackAllowlist ?? ALLOW_EVERY_CALLER;
    }
    return createCallerAllowlist(parseAllowListEnv(raw)).allowlist;
  }
}
