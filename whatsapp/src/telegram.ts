// Telegram accounts (teleproto, a maintained fork of GramJS: Telegram's own protocol, no
// browser), signed in as a person, by scanning a QR code from the Telegram app like WhatsApp
// Web's. They ask bots (the price-list bot, which answers on Telegram only) and nothing else.
//
// The list of accounts lives in /data/telegram/accounts.json, each one's login (a session
// string) in /data/telegram/<id>.session, so a restart needs no new scan. Ids start with
// "tg-": the Callora server tells a Telegram account from a WhatsApp one by it.

import { randomUUID } from "node:crypto";
import { mkdir, readFile, rename, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import QRCode from "qrcode";
import { Api, TelegramClient } from "teleproto";
import { EditedMessage, NewMessage } from "teleproto/events";
import type { EditedMessageEvent, NewMessageEvent } from "teleproto/events";
import { StringSession } from "teleproto/sessions";
import { NoReply, NotAllowed, NotFound, NotReady, OutcomeUnknown } from "./sessions.js";

const DATA = path.join(process.env.DATA_DIR ?? "/data", "telegram");
const LIST = path.join(DATA, "accounts.json");
const API_ID = Number(process.env.TELEGRAM_API_ID ?? 0);
const API_HASH = process.env.TELEGRAM_API_HASH ?? "";
const MAX_ACCOUNTS = Number(process.env.TELEGRAM_MAX_ACCOUNTS ?? 3);
/** How long a question may wait for its answer, at most. */
const MAX_ASK_MS = 30_000;

/** How often a signed-in account checks that Telegram still knows its login. */
const CHECK_EVERY_MS = 60_000;

/** Telegram no longer knows the login: ended from the phone (Settings > Devices), or by
 * Telegram itself. A live account showed "ready" for an hour after this, and every question
 * to the price bot failed as "not a contact". */
function loginGone(e: unknown): boolean {
  const m = e instanceof Error ? `${e.name} ${e.message}` : String(e);
  return /AUTH_KEY_UNREGISTERED|AuthKeyUnregistered|SESSION_REVOKED|SESSION_EXPIRED|USER_DEACTIVATED|AUTH_KEY_DUPLICATED/i.test(m);
}

/** Whether the server has the API id and hash Telegram requires (my.telegram.org). */
export const telegramConfigured = API_ID > 0 && API_HASH.length > 0;

type Status = "starting" | "qr" | "password" | "ready" | "disconnected" | "failed";
type Stored = { id: string; name: string; created_at: string; first_ready_at: string | null };

function log(id: string, ...what: unknown[]) {
  console.log(JSON.stringify({ at: new Date().toISOString(), telegram: id, message: what.map(String).join(" ") }));
}

class Account {
  status: Status = "starting";
  /** The current sign-in link (tg://login?token=…); a new one every half minute. */
  link: string | null = null;
  /** The two-step password's hint, while it is asked for. */
  hint: string | null = null;
  me: { number: string; name: string; username: string } | null = null;
  error: string | null = null;
  client: TelegramClient | null = null;
  private password: ((p: string) => void) | null = null;
  private abort: AbortController | null = null;
  private check: NodeJS.Timeout | null = null;
  private generation = 0;
  private readers = new Set<(m: { id: number; from: string; text: string }) => void>();

  constructor(
    public stored: Stored,
    private onChange: () => void,
  ) {}

  private file() {
    return path.join(DATA, `${this.stored.id}.session`);
  }

  listen(reader: (m: { id: number; from: string; text: string }) => void): () => void {
    this.readers.add(reader);
    return () => this.readers.delete(reader);
  }

  async start() {
    const generation = ++this.generation;
    await this.stop();
    this.status = "starting";
    this.link = null;
    this.hint = null;
    this.error = null;
    const saved = await readFile(this.file(), "utf8").catch(() => "");
    const client = new TelegramClient(new StringSession(saved), API_ID, API_HASH, {
      connectionRetries: 5,
      deviceModel: "Callora",
      appVersion: "1.0",
    });
    this.client = client;
    const current = () => generation === this.generation;
    try {
      await client.connect();
      if (!(await client.checkAuthorization())) {
        this.abort = new AbortController();
        await client.signInUserWithQrCode(
          { apiId: API_ID, apiHash: API_HASH },
          {
            qrCode: async ({ token }) => {
              if (!current()) return;
              this.status = "qr";
              this.link = `tg://login?token=${token.toString("base64url")}`;
            },
            password: (hint?: string) =>
              new Promise<string>((resolve) => {
                if (!current()) return;
                this.status = "password";
                this.hint = hint ?? null;
                this.link = null;
                this.password = resolve;
              }),
            onError: async (e: Error) => {
              if (!current()) return true;
              // A wrong two-step password: asked again, with the reason; anything else ends the try.
              if (/PASSWORD_HASH_INVALID/.test(e.message)) {
                this.error = "wrong_password";
                return false;
              }
              this.error = e.message;
              log(this.stored.id, "sign-in error", e.message);
              return true;
            },
            abortSignal: this.abort.signal,
          },
        );
      }
      if (!current()) return;
      await writeFile(this.file(), String(client.session.save()), { mode: 0o600 });
      const me = (await client.getMe()) as Api.User;
      this.me = {
        number: me.phone ?? "",
        name: [me.firstName, me.lastName].filter(Boolean).join(" "),
        username: me.username ?? "",
      };
      // Known chats: a bot is then found by its id as well as by its @name.
      await client.getDialogs({ limit: 200 }).catch(() => undefined);
      const deliver = (m: Api.Message | undefined) => {
        if (!m || m.out || !current()) return;
        const from = String(m.senderId ?? m.chatId ?? "");
        for (const reader of this.readers) reader({ id: m.id, from, text: m.message ?? "" });
      };
      client.addEventHandler((e: NewMessageEvent) => deliver(e.message), new NewMessage({}));
      // Bots often write "calculating…" and then edit the message into the answer.
      client.addEventHandler((e: EditedMessageEvent) => deliver(e.message), new EditedMessage({}));
      this.status = "ready";
      this.error = null;
      this.check = setInterval(() => {
        if (!current() || this.status !== "ready") return;
        client.invoke(new Api.updates.GetState()).catch((e: unknown) => {
          if (current() && loginGone(e)) void this.gone();
        });
      }, CHECK_EVERY_MS);
      if (!this.stored.first_ready_at) {
        this.stored.first_ready_at = new Date().toISOString();
        this.onChange();
      }
      log(this.stored.id, "ready", this.me.username || this.me.number);
    } catch (e) {
      if (!current()) return;
      this.status = "failed";
      this.error = e instanceof Error ? e.message : String(e);
      log(this.stored.id, "failed", this.error);
    }
  }

  /** Telegram ended the login: the account shows as disconnected, to be scanned again. */
  async gone() {
    log(this.stored.id, "Telegram ended the login");
    this.generation++;
    await this.stop();
    await rm(this.file(), { force: true });
    this.status = "disconnected";
    this.me = null;
    this.error = "telegram_ended_the_login";
  }

  /** An error of a call to Telegram: the login gone makes the account disconnected. */
  async failed(e: unknown): Promise<never> {
    if (loginGone(e)) {
      await this.gone();
      throw new NotReady("not_ready");
    }
    throw e;
  }

  /** The two-step password, while it is asked for. */
  givePassword(password: string) {
    if (this.status !== "password" || !this.password) throw new NotAllowed("no_password_asked");
    const resolve = this.password;
    this.password = null;
    this.status = "starting";
    this.error = null;
    resolve(password);
  }

  async stop() {
    if (this.check) clearInterval(this.check);
    this.check = null;
    this.abort?.abort();
    this.abort = null;
    this.password = null;
    const client = this.client;
    this.client = null;
    if (client) await client.destroy().catch(() => undefined);
  }

  async logOut() {
    if (this.client && this.status === "ready") await this.client.logOut().catch(() => undefined);
    await this.stop();
    await rm(this.file(), { force: true });
  }

  ready(): TelegramClient {
    if (this.status !== "ready" || !this.client) throw new NotReady("not_ready");
    return this.client;
  }

  view() {
    return {
      id: this.stored.id,
      name: this.stored.name,
      created_at: this.stored.created_at,
      first_ready_at: this.stored.first_ready_at,
      status: this.status,
      me: this.me,
      hint: this.status === "password" ? this.hint : null,
      error: this.status === "ready" ? null : this.error,
    };
  }
}

export class TelegramAccounts {
  private accounts = new Map<string, Account>();
  private asking = new Map<string, Promise<unknown>>();

  async load() {
    await mkdir(DATA, { recursive: true });
    if (!telegramConfigured) return;
    let stored: Stored[] = [];
    try {
      stored = JSON.parse(await readFile(LIST, "utf8")) as Stored[];
    } catch {
      /* first start */
    }
    for (const s of stored) {
      const a = new Account(s, () => void this.save());
      this.accounts.set(s.id, a);
      void a.start();
    }
  }

  private async save() {
    const tmp = `${LIST}.tmp`;
    await writeFile(tmp, JSON.stringify([...this.accounts.values()].map((a) => a.stored), null, 2));
    await rename(tmp, LIST);
  }

  list() {
    return { configured: telegramConfigured, max: MAX_ACCOUNTS, sessions: [...this.accounts.values()].map((a) => a.view()) };
  }

  get(id: string): Account {
    const a = this.accounts.get(id);
    if (!a) throw new NotFound("not_found");
    return a;
  }

  async create(name: string) {
    if (!telegramConfigured) throw new NotAllowed("telegram_not_configured");
    if (this.accounts.size >= MAX_ACCOUNTS) throw new NotAllowed("too_many_sessions");
    const stored: Stored = {
      id: `tg-${randomUUID().slice(0, 8)}`,
      name: name.slice(0, 60) || "טלגרם",
      created_at: new Date().toISOString(),
      first_ready_at: null,
    };
    const a = new Account(stored, () => void this.save());
    this.accounts.set(stored.id, a);
    await this.save();
    void a.start();
    return a.view();
  }

  async restart(id: string) {
    const a = this.get(id);
    void a.start();
    return a.view();
  }

  /** Signs the account out of Telegram and forgets it. */
  async remove(id: string) {
    const a = this.get(id);
    await a.logOut();
    this.accounts.delete(id);
    await this.save();
  }

  /** The sign-in QR code (or that the password is asked for). */
  async qr(id: string) {
    const a = this.get(id);
    return {
      status: a.status,
      hint: a.status === "password" ? a.hint : null,
      error: a.error,
      qr: a.link ? await QRCode.toDataURL(a.link, { margin: 1, width: 280 }) : null,
    };
  }

  password(id: string, password: string) {
    this.get(id).givePassword(password);
    return this.get(id).view();
  }

  /** The account's chats, in the shape the WhatsApp service gives: groups, and people and
   * bots as "contacts" (a bot by its @name, which is what it is asked by). */
  async chats(id: string) {
    const account = this.get(id);
    const client = account.ready();
    const dialogs = await client.getDialogs({ limit: 200 }).catch((e: unknown) => account.failed(e));
    const groups: { id: string; name: string }[] = [];
    const contacts: { id: string; name: string; number: string; bot: boolean }[] = [];
    for (const d of dialogs) {
      const e = d.entity as (Api.User & Api.Chat & Api.Channel) | undefined;
      if (!e || !d.id) continue;
      if (d.isUser) {
        const user = e as unknown as Api.User;
        if (user.self) continue;
        contacts.push({
          id: user.username ? `@${user.username}` : String(d.id),
          name: d.title || user.username || String(d.id),
          number: user.phone ? user.phone : user.username ? `@${user.username}` : "",
          bot: Boolean(user.bot),
        });
      } else if (d.isGroup || d.isChannel) {
        groups.push({ id: String(d.id), name: d.title ?? String(d.id) });
      }
    }
    const byName = (a: { name: string }, b: { name: string }) => a.name.localeCompare(b.name, "he");
    // Bots first: the price-list bot is one.
    contacts.sort((a, b) => Number(b.bot) - Number(a.bot) || byName(a, b));
    return { groups: groups.sort(byName), contacts };
  }

  async send(id: string, chatId: string, text: string, typingMs: number) {
    const account = this.get(id);
    const client = account.ready();
    const peer = await client.getInputEntity(chatId).catch(async (e: unknown) => {
      if (loginGone(e)) return account.failed(e);
      throw new NotAllowed("not_a_contact");
    });
    if (typingMs > 0) {
      await client
        .invoke(new Api.messages.SetTyping({ peer, action: new Api.SendMessageTypingAction() }))
        .catch(() => undefined);
      await new Promise((r) => setTimeout(r, Math.min(typingMs, 5000)));
    }
    try {
      const m = await client.sendMessage(peer, { message: text });
      return { id: String(m.id) };
    } catch (e) {
      if (loginGone(e)) return account.failed(e);
      log(id, "send outcome unknown", e instanceof Error ? e.message : String(e));
      throw new OutcomeUnknown("outcome_unknown");
    }
  }

  /** Asks a bot and collects what it writes back, as the WhatsApp service does: one question
   * at a time in each chat, until a message has `until` in it and `quietMs` pass with nothing
   * more, or `timeoutMs`. A message the bot edits counts as its edited text. */
  ask(id: string, a: { chatId: string; text: string; timeoutMs: number; quietMs: number; until: string; typingMs?: number }) {
    const key = `${id} ${a.chatId}`;
    const before = this.asking.get(key) ?? Promise.resolve();
    const mine = before.catch(() => undefined).then(() => this.askNow(id, a));
    const tail = mine.catch(() => undefined);
    this.asking.set(key, tail);
    void tail.then(() => {
      if (this.asking.get(key) === tail) this.asking.delete(key);
    });
    return mine;
  }

  private async askNow(
    id: string,
    a: { chatId: string; text: string; timeoutMs: number; quietMs: number; until: string; typingMs?: number },
  ): Promise<{ replies: string[] }> {
    const account = this.get(id);
    const client = account.ready();
    const entity = await client.getEntity(a.chatId).catch(async (e: unknown) => {
      if (loginGone(e)) return account.failed(e);
      throw new NotAllowed("not_a_contact");
    });
    const bot = String(entity.id);
    const replies = new Map<number, string>();
    let answered = false;
    let finish: () => void = () => undefined;
    const finished = new Promise<void>((resolve) => (finish = resolve));
    let quiet: NodeJS.Timeout | null = null;
    const stop = account.listen((m) => {
      if (m.from !== bot || !m.text.trim()) return;
      replies.set(m.id, m.text);
      if (!a.until || m.text.includes(a.until)) answered = true;
      if (answered) {
        if (quiet) clearTimeout(quiet);
        quiet = setTimeout(finish, a.quietMs);
      }
    });
    const timer = setTimeout(finish, Math.min(a.timeoutMs, MAX_ASK_MS));
    try {
      await this.send(id, a.chatId, a.text, a.typingMs ?? 0);
      await finished;
    } finally {
      stop();
      clearTimeout(timer);
      if (quiet) clearTimeout(quiet);
    }
    if (!answered) throw new NoReply("no_reply");
    return { replies: [...replies.entries()].sort((x, y) => x[0] - y[0]).map(([, t]) => t) };
  }
}
