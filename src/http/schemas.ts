import { z } from 'zod';
import { REALTIME_PROVIDERS } from '../realtime/provider.js';

export const e164Schema = z.string().regex(/^\+[1-9]\d{7,14}$/, 'must be a valid E.164 phone number');

export const createBusinessSchema = z.object({
  name: z.string().trim().min(1).max(120),
  phoneNumber: e164Schema,
  greeting: z.string().trim().min(1).max(500),
  active: z.boolean().default(true),
});

export const updateBusinessSchema = createBusinessSchema.partial().refine(
  (input) => Object.keys(input).length > 0,
  'at least one field is required',
);

export const idParamsSchema = z.object({ id: z.uuid() });

export const callsQuerySchema = z.object({
  businessId: z.uuid().optional(),
  limit: z.coerce.number().int().min(1).max(100).default(50),
  offset: z.coerce.number().int().min(0).default(0),
});

/**
 * Agent configuration written from the control plane. `voice` and `realtimeModel` are
 * interpreted by the selected provider, so they stay free text: pinning them to an
 * enumeration would make Callora lag every provider's model releases.
 */
export const agentConfigSchema = z
  .object({
    enabled: z.boolean().default(true),
    language: z.string().trim().min(2).max(16),
    greeting: z.string().trim().min(1).max(500),
    instructions: z.string().trim().min(1).max(8000),
    voiceProvider: z.enum([...REALTIME_PROVIDERS]),
    // Blank means the platform-wide agent, so an existing deployment keeps its behaviour.
    elevenLabsAgentId: z.string().trim().max(120).default(''),
    // Blank is meaningful: it means "use whatever the provider is already configured with".
    voice: z.string().trim().max(80).default(''),
    realtimeModel: z.string().trim().min(1).max(80),
  })
  .superRefine((input, ctx) => {
    // OpenAI Realtime has no voice of its own to fall back on: the session names one.
    // ElevenLabs and Cartesia both have a configured default, so blank is valid there.
    if (input.voiceProvider === 'openai' && input.voice === '') {
      ctx.addIssue({
        code: 'custom',
        path: ['voice'],
        message: 'a voice is required for the OpenAI provider',
      });
    }
  });

export const auditQuerySchema = z.object({
  entityType: z.string().trim().min(1).max(40).optional(),
  entityId: z.string().trim().min(1).max(80).optional(),
  limit: z.coerce.number().int().min(1).max(200).default(50),
  offset: z.coerce.number().int().min(0).default(0),
});

export const loginSchema = z.object({
  email: z.string().trim().min(3).max(320),
  password: z.string().min(1).max(200),
});

export const changePasswordSchema = z
  .object({
    currentPassword: z.string().min(1).max(200),
    newPassword: z.string().min(12, 'must be at least 12 characters').max(200),
    confirmPassword: z.string().min(1).max(200),
  })
  .refine((input) => input.newPassword === input.confirmPassword, {
    path: ['confirmPassword'],
    message: 'the new passwords do not match',
  });

export const incomingCallSchema = z.object({
  To: e164Schema,
  From: z.string().optional(),
  CallSid: z.string().min(1),
  CallStatus: z.string().min(1).default('ringing'),
  Direction: z.string().min(1).optional(),
});

export const callStatusSchema = z.object({
  To: e164Schema,
  CallSid: z.string().min(1),
  CallStatus: z.string().min(1),
  CallDuration: z.coerce.number().int().min(0).optional(),
});
