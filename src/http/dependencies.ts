import type { AuthService } from '../auth/sessions.js';
import type { AppConfig } from '../config.js';
import type { DataStore } from '../db/store.js';
import type { CallerAllowlist } from '../dev/caller-allowlist.js';
import type { CallTerminator } from '../telephony/call-terminator.js';
import type { AuditRecorder } from './audit.js';

export interface RouteDependencies {
  config: AppConfig;
  store: DataStore;
  /** Overridable so tests never reach the Twilio REST API. */
  callTerminator?: CallTerminator;
  /** Development-only caller gate; absent means every caller is allowed. */
  callerAllowlist?: CallerAllowlist;
}

/** Everything the control-plane routes need, built once in `buildApp`. */
export interface ControlPlaneDependencies extends RouteDependencies {
  auth: AuthService;
  audit: AuditRecorder;
}
