// The admin API, called with the session cookie the login set.

import { useCallback, useEffect, useRef, useState } from "react";

export type Usage = { model: string; input: number; cached: number; output: number };

export type Verdict = "good" | "bad";

export type CallRow = {
  id: string;
  call_sid: string;
  business_id: string;
  from: string | null;
  to: string;
  started_at: string;
  ended_at: string | null;
  outcome: string | null;
  twilio_status: string | null;
  duration_seconds: number | null;
  usage: Usage | null;
  verdict: Verdict | null;
  orders: number;
};

export type Stats = {
  days: number;
  calls: number;
  done_without_a_person: number;
  done_without_a_person_share: number | null;
  handed_off: number;
  handed_off_share: number | null;
  nothing_done: number;
  to_verify: number;
  reviewed_good: number;
  reviewed_bad: number;
  avg_duration_seconds: number | null;
  booking_seconds_median: number | null;
  booking_turns_median: number | null;
  cost: number | null;
  cost_per_call: number | null;
};

export type Day = { day: string; calls: number; done: number; handed_off: number };

export type Field = { slot: string; value: string };

export type AgentReply = {
  action?: string;
  task?: string | null;
  fields?: Field[];
  asks?: string[];
  phrase?: string | null;
  say?: string;
};

export type TurnDetail = {
  route?: string;
  reply?: AgentReply | null;
  decision_ms?: number;
  error?: string | null;
  second_hearing?: string | null;
  held_for_open_question?: string | null;
  transcript?: string;
};

export type Turn = { speaker: "caller" | "agent"; text: string; detail: TurnDetail | null; at: string };

export type ActionRun = {
  action: string;
  ok: boolean;
  latency_ms: number;
  at: string;
  result: { error?: { outcome_unknown?: boolean } } | null;
};

export type CallDetail = {
  id: string;
  call_sid: string;
  from: string | null;
  to: string;
  started_at: string;
  ended_at: string | null;
  outcome: string | null;
  usage: Usage | null;
  review: { verdict: Verdict; note: string } | null;
  turns: Turn[];
  actions: ActionRun[];
  handoffs: { reason: string; summary: { text?: string } | null; at: string }[];
  utterances: { id: number; heard: string; at: string }[];
};

export type OrderDetail = { field: string; label: string; value: string; address: string | null };

export type Order = {
  at: string;
  from: string | null;
  call_id: string;
  card: {
    task: string;
    phone?: string | null;
    verify?: boolean;
    result?: Record<string, unknown> | null;
    details: OrderDetail[];
  };
};

export type DeskSettings = { numbers: string[]; caller_id?: string | null; hold_music: string; max_wait_seconds: number };

export type PriceBotSettings = { account: string; chat_id: string; chat_name: string };

export type VoiceChoice = { id: string; name: string; description: string; preview?: string | null };

export type VoiceSettings = {
  /** The voice calls start with now. */
  active: string | null;
  /** The voice the owner chose (it becomes active once recorded). */
  chosen: string | null;
  default: string | null;
  choices: VoiceChoice[];
  building: { voice: string; error: string | null } | null;
  clips: number;
};

export type AgentModelChoice = { primary: string; backup: string; effort: string | null };

/** The model that runs the agent, as the settings page shows it. */
export type AgentModelData = {
  active: AgentModelChoice;
  /** What the server's environment says: where "back to default" goes. */
  defaults: AgentModelChoice;
  custom: boolean;
  /** Which providers the server has a key for. */
  providers: { gemini: boolean; openai: boolean };
  models: { id: string; provider: "gemini" | "openai" }[];
};

export type SettingsData = {
  businesses: { id: string; name: string; desk: DeskSettings; price_bot: PriceBotSettings | null; voice: VoiceSettings }[];
  music: string[];
  saving: boolean;
  transfers: boolean;
  whatsapp: boolean;
  voice_switching: boolean;
  /** None when the server has no key for the agent's model. */
  agent: AgentModelData | null;
};

export type Session = { businesses: { id: string; name: string }[]; database: boolean };

/** Raised for a 401: the page asks for the password again. */
export class Unauthorized extends Error {}

export async function api<T>(path: string, init: RequestInit = {}): Promise<T> {
  const headers = new Headers(init.headers);
  if (init.body && !headers.has("content-type")) headers.set("content-type", "application/json");
  const res = await fetch(path, { ...init, headers, credentials: "same-origin" });
  if (res.status === 401) {
    window.dispatchEvent(new Event("callora:unauthorized"));
    throw new Unauthorized("unauthorized");
  }
  if (!res.ok) throw new Error(`${res.status}`);
  if (res.status === 204) return undefined as T;
  return (await res.json()) as T;
}

/** Data from the API, reloaded when `path` changes and, if asked, every `refreshMs`. */
export function useApi<T>(path: string | null, refreshMs?: number) {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(path !== null);
  const current = useRef(path);
  current.current = path;

  const load = useCallback(
    async (quiet = false) => {
      if (!path) return;
      if (!quiet) setLoading(true);
      try {
        const value = await api<T>(path);
        if (current.current === path) {
          setData(value);
          setError(null);
        }
      } catch (e) {
        if (!(e instanceof Unauthorized) && current.current === path) {
          setError(e instanceof Error && e.message === "503" ? "אין חיבור למסד הנתונים" : "הנתונים לא נטענו");
        }
      } finally {
        if (current.current === path) setLoading(false);
      }
    },
    [path],
  );

  // Another path is other data: what the last one showed must not stand for it while it loads,
  // nor when it fails (a Telegram account's bots were searched in a WhatsApp contact list).
  const shown = useRef(path);
  if (shown.current !== path) {
    shown.current = path;
    setData(null);
    setError(null);
  }

  useEffect(() => {
    void load();
    if (!refreshMs) return;
    const timer = window.setInterval(() => {
      if (document.visibilityState === "visible") void load(true);
    }, refreshMs);
    return () => window.clearInterval(timer);
  }, [load, refreshMs]);

  return { data, error, loading, reload: () => load(true) };
}
