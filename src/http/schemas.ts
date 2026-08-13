import { z } from 'zod';

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
