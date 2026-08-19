import type { AuthService } from '../auth/sessions.js';
import type { AppConfig } from '../config.js';
import type { DataStore } from '../db/store.js';
import type { CallerAllowlist } from '../dev/caller-allowlist.js';
import type { PlatformSettings } from '../platform/settings.js';
import type { CallTerminator } from '../telephony/call-terminator.js';
import type { AuditRecorder } from './audit.js';

export interface RouteDependencies {
  config: AppConfig;
  store: DataStore;
  /** Overridable so tests never reach the Twilio REST API. */
  callTerminator?: CallTerminator;
  /**
   * Caller gate loaded at startup from the gitignored local file. It is the fallback when
   * no allowlist has been entered in the dashboard; absent means every caller is allowed.
   */
  callerAllowlist?: CallerAllowlist;
  /** Overridable so tests can seed platform settings; `buildApp` builds one otherwise. */
  platform?: PlatformSettings;
  /** Outbound HTTP for provider management calls; injectable so tests stay offline. */
  fetchImpl?: typeof fetch;
}

/** Everything the control-plane routes need, built once in `buildApp`. */
export interface ControlPlaneDependencies extends RouteDependencies {
  auth: AuthService;
  audit: AuditRecorder;
  /**
   * The live platform configuration. Routes read provider credentials from here rather
   * than from `config`, so a credential saved in the dashboard applies to the next call.
   */
  platform: PlatformSettings;
}
