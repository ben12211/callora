import type { FastifyBaseLogger } from 'fastify';
import type { AuthenticatedActor } from '../auth/sessions.js';
import type { DataStore } from '../db/store.js';
import type { AgentConfig, Business } from '../domain/models.js';

/**
 * Administrative history. Every control-plane write that changes what a caller hears —
 * business creation, activation, agent and provider changes, password changes — leaves a
 * row here. Recording is best effort: an audit failure must never fail the write itself,
 * but it is logged loudly.
 */
export const AUDIT_ACTIONS = {
  businessCreated: 'business.created',
  businessUpdated: 'business.updated',
  businessEnabled: 'business.enabled',
  businessDisabled: 'business.disabled',
  businessDeleted: 'business.deleted',
  agentUpdated: 'agent.updated',
  adminLoggedIn: 'admin.logged_in',
  adminPasswordChanged: 'admin.password_changed',
} as const;

export type AuditAction = (typeof AUDIT_ACTIONS)[keyof typeof AUDIT_ACTIONS];

export interface AuditWrite {
  action: AuditAction;
  entityType: 'business' | 'agent' | 'admin';
  entityId: string | null;
  summary: string;
  details?: Record<string, unknown>;
}

export class AuditRecorder {
  public constructor(
    private readonly store: DataStore,
    private readonly logger: FastifyBaseLogger,
  ) {}

  public async record(actor: AuthenticatedActor | undefined, write: AuditWrite): Promise<void> {
    try {
      await this.store.recordAuditEvent({
        actorId: actor?.id ?? null,
        actorLabel: actor?.label ?? 'unknown',
        action: write.action,
        entityType: write.entityType,
        entityId: write.entityId,
        summary: write.summary,
        ...(write.details ? { details: write.details } : {}),
      });
    } catch (error) {
      this.logger.error({ error, action: write.action }, 'Failed to record an audit event');
    }
  }
}

/** Field-level diff for the audit detail, with nothing sensitive to redact in these tables. */
export function changedFields<T extends object>(
  before: T | null,
  after: T,
  fields: readonly (keyof T & string)[],
): Record<string, { from: unknown; to: unknown }> {
  const changes: Record<string, { from: unknown; to: unknown }> = {};
  for (const field of fields) {
    const from = before ? before[field] : undefined;
    if (from !== after[field]) {
      changes[field] = { from: from ?? null, to: after[field] };
    }
  }
  return changes;
}

export const BUSINESS_AUDIT_FIELDS = ['name', 'phoneNumber', 'greeting', 'active'] as const satisfies
  readonly (keyof Business & string)[];

export const AGENT_AUDIT_FIELDS = [
  'enabled',
  'language',
  'greeting',
  'instructions',
  'voiceProvider',
  'voice',
  'realtimeModel',
] as const satisfies readonly (keyof AgentConfig & string)[];
