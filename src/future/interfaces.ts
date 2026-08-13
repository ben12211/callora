/**
 * Extension seams reserved for later pushes. They intentionally have no implementation yet.
 */

export interface CallContext {
  callId: string;
  businessId: string;
}

export interface RealtimeAiSession {
  start(context: CallContext): Promise<void>;
  stop(): Promise<void>;
}

export interface MediaStreamTransport {
  attach(context: CallContext): Promise<void>;
  close(): Promise<void>;
}

export interface ToolCall {
  name: string;
  arguments: unknown;
}

export interface BusinessToolProvider {
  execute(call: ToolCall, context: CallContext): Promise<unknown>;
}

export interface CustomerOperationsProvider {
  lookupCustomer(phoneNumber: string): Promise<unknown>;
  lookupOrder(orderId: string): Promise<unknown>;
  manageAppointment(input: unknown): Promise<unknown>;
}

export interface WhatsAppGateway {
  sendMessage(to: string, body: string): Promise<void>;
}

export interface VoiceProvider {
  synthesize(text: string, voiceId: string): Promise<Uint8Array>;
}
