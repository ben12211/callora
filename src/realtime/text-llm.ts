/**
 * Minimal streaming client for an OpenAI-compatible chat completions endpoint.
 *
 * The Cartesia pipeline is the only provider where Callora owns the reasoning turn:
 * OpenAI Realtime and ElevenLabs Agents each run their own model. Deltas are surfaced
 * as they arrive so they can be pushed straight into Sonic, rather than buffering a
 * whole reply and paying for its latency twice.
 *
 * Written against `fetch` and hand-parsed SSE rather than an SDK: the project has no
 * OpenAI dependency, and this needs exactly one endpoint.
 */

export interface ChatMessage {
  role: 'system' | 'user' | 'assistant' | 'tool';
  content: string;
  tool_call_id?: string;
  tool_calls?: ChatToolCall[];
}

export interface ChatToolCall {
  id: string;
  type: 'function';
  function: { name: string; arguments: string };
}

export interface ChatToolDefinition {
  type: 'function';
  function: {
    name: string;
    description: string;
    parameters: Record<string, unknown>;
  };
}

export interface StreamChatOptions {
  baseUrl: string;
  apiKey: string;
  model: string;
  messages: readonly ChatMessage[];
  tools?: readonly ChatToolDefinition[];
  /** Aborts the HTTP request outright; used for barge-in. */
  signal?: AbortSignal;
  /** Called for every text fragment, in order, as it arrives. */
  onTextDelta?: (delta: string) => void;
  fetchImpl?: typeof fetch;
}

export interface StreamChatResult {
  /** The full assistant reply, concatenated from the deltas. */
  text: string;
  toolCalls: ChatToolCall[];
  /** True when the caller aborted the stream mid-flight. */
  aborted: boolean;
}

interface StreamedToolCall {
  id: string;
  name: string;
  arguments: string;
}

/** Accumulates the `tool_calls` deltas, which arrive split across chunks by index. */
function applyToolCallDelta(accumulator: Map<number, StreamedToolCall>, delta: Record<string, unknown>): void {
  const index = typeof delta['index'] === 'number' ? delta['index'] : 0;
  const existing = accumulator.get(index) ?? { id: '', name: '', arguments: '' };
  const fn = delta['function'];

  if (typeof delta['id'] === 'string') {
    existing.id = delta['id'];
  }
  if (typeof fn === 'object' && fn !== null) {
    const shape = fn as { name?: unknown; arguments?: unknown };
    if (typeof shape.name === 'string') {
      existing.name += shape.name;
    }
    if (typeof shape.arguments === 'string') {
      existing.arguments += shape.arguments;
    }
  }
  accumulator.set(index, existing);
}

function parseChunk(
  payload: string,
  accumulator: Map<number, StreamedToolCall>,
  onTextDelta?: (delta: string) => void,
): string {
  let text = '';
  let parsed: unknown;
  try {
    parsed = JSON.parse(payload);
  } catch {
    // A partial or non-JSON keepalive line is not worth failing a live call over.
    return text;
  }

  const choices = (parsed as { choices?: unknown }).choices;
  if (!Array.isArray(choices)) {
    return text;
  }

  for (const choice of choices) {
    const delta = (choice as { delta?: unknown }).delta;
    if (typeof delta !== 'object' || delta === null) {
      continue;
    }
    const content = (delta as { content?: unknown }).content;
    if (typeof content === 'string' && content.length > 0) {
      text += content;
      onTextDelta?.(content);
    }
    const toolCalls = (delta as { tool_calls?: unknown }).tool_calls;
    if (Array.isArray(toolCalls)) {
      for (const entry of toolCalls) {
        if (typeof entry === 'object' && entry !== null) {
          applyToolCallDelta(accumulator, entry as Record<string, unknown>);
        }
      }
    }
  }
  return text;
}

/**
 * Streams one assistant turn. Resolves with whatever was produced before the stream
 * ended — including on abort, so a barged-in turn can still be recorded as partially
 * spoken rather than lost from the conversation history.
 */
export async function streamChatCompletion(options: StreamChatOptions): Promise<StreamChatResult> {
  const { baseUrl, apiKey, model, messages, tools, signal, onTextDelta, fetchImpl = fetch } = options;

  let response: Response;
  try {
    response = await fetchImpl(`${baseUrl}/chat/completions`, {
      method: 'POST',
      headers: { 'content-type': 'application/json', authorization: `Bearer ${apiKey}` },
      body: JSON.stringify({
        model,
        messages,
        stream: true,
        ...(tools && tools.length > 0 ? { tools, tool_choice: 'auto' } : {}),
      }),
      signal,
    });
  } catch (error) {
    if (signal?.aborted) {
      return { text: '', toolCalls: [], aborted: true };
    }
    throw error;
  }

  if (!response.ok) {
    // Status only: the body can echo request content, and the key is in the headers.
    throw new Error(`The text LLM rejected the request with status ${response.status}`);
  }
  if (!response.body) {
    throw new Error('The text LLM returned no response body');
  }

  const accumulator = new Map<number, StreamedToolCall>();
  const decoder = new TextDecoder();
  const reader = response.body.getReader();
  let buffer = '';
  let text = '';
  let aborted = false;

  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) {
        break;
      }
      buffer += decoder.decode(value, { stream: true });

      // SSE frames are separated by a blank line; a frame may span several reads.
      let boundary = buffer.indexOf('\n\n');
      while (boundary !== -1) {
        const frame = buffer.slice(0, boundary);
        buffer = buffer.slice(boundary + 2);

        for (const line of frame.split('\n')) {
          if (!line.startsWith('data:')) {
            continue;
          }
          const payload = line.slice(5).trim();
          if (payload === '' || payload === '[DONE]') {
            continue;
          }
          text += parseChunk(payload, accumulator, onTextDelta);
        }
        boundary = buffer.indexOf('\n\n');
      }
    }
  } catch (error) {
    if (signal?.aborted) {
      aborted = true;
    } else {
      throw error;
    }
  } finally {
    reader.cancel().catch(() => {
      // The stream is already being torn down; nothing further to do.
    });
  }

  const toolCalls: ChatToolCall[] = [...accumulator.entries()]
    .sort(([left], [right]) => left - right)
    .filter(([, call]) => call.name.length > 0)
    .map(([, call]) => ({
      id: call.id,
      type: 'function' as const,
      function: { name: call.name, arguments: call.arguments },
    }));

  return { text, toolCalls, aborted: aborted || Boolean(signal?.aborted) };
}
