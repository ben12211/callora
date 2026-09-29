// WhatsApp accounts, each a whatsapp-web.js client signed in by scanning its own QR code.
// The list of accounts and when each was first connected live in /data/sessions.json; each
// account's login lives under /data/auth/session-<id>, so a restart needs no new scan.

import { randomUUID } from "node:crypto";
import { mkdir, readdir, readFile, rename, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import QRCode from "qrcode";
import pkg from "whatsapp-web.js";
import type { Client as ClientType } from "whatsapp-web.js";

const { Client, LocalAuth } = pkg;

const DATA = process.env.DATA_DIR ?? "/data";
const AUTH = path.join(DATA, "auth");
const LIST = path.join(DATA, "sessions.json");
const CHROMIUM = process.env.CHROMIUM_PATH ?? "/usr/bin/chromium";
export const MAX_SESSIONS = Number(process.env.MAX_SESSIONS ?? 5);

export type Status = "starting" | "qr" | "authenticating" | "ready" | "disconnected" | "failed";

type Stored = { id: string; name: string; created_at: string; first_ready_at: string | null };

export class NotAllowed extends Error {}
export class NotReady extends Error {}
export class NotFound extends Error {}
/** The message was handed to WhatsApp and something failed after: it may well be out, so it
 * is never sent again on its own (a message sent again and again reached a group 8 times). */
export class OutcomeUnknown extends Error {}

class Session {
  status: Status = "starting";
  qr: string | null = null;
  me: { number: string; name: string } | null = null;
  error: string | null = null;
  client: ClientType | null = null;
  /** Each start is a generation; events of a browser that was replaced are ignored. */
  private generation = 0;
  private starting: Promise<void> | null = null;
  private retried = false;

  constructor(
    public stored: Stored,
    private onChange: () => void,
  ) {}

  get busy(): boolean {
    return this.starting !== null;
  }

  /** One start at a time: a second call while starting waits for the first. */
  start(): Promise<void> {
    if (!this.starting) this.starting = this.run().finally(() => (this.starting = null));
    return this.starting;
  }

  private async run() {
    const generation = ++this.generation;
    // The previous browser must be gone, or the new one finds its profile locked.
    await this.stop();
    await freeProfile(this.stored.id);
    this.status = "starting";
    this.qr = null;
    this.error = null;
    const client = new Client({
      authStrategy: new LocalAuth({ clientId: this.stored.id, dataPath: AUTH }),
      // WhatsApp Web's page is kept where the service may write: under /app it cannot, and
      // whatsapp-web.js then stops after the scan without a word ("authenticating" forever).
      webVersionCache: { type: "local", path: path.join(DATA, "web-cache") },
      puppeteer: {
        executablePath: CHROMIUM,
        headless: true,
        // Inside a container: no sandbox user namespaces, a small /dev/shm.
        args: ["--no-sandbox", "--disable-setuid-sandbox", "--disable-dev-shm-usage", "--disable-gpu", "--no-first-run", "--no-zygote"],
      },
    });
    this.client = client;
    const current = () => generation === this.generation;
    client.on("qr", (qr: string) => {
      if (!current()) return;
      this.status = "qr";
      this.qr = qr;
    });
    client.on("authenticated", () => {
      if (!current()) return;
      this.status = "authenticating";
      this.qr = null;
    });
    client.on("ready", () => {
      if (!current()) return;
      this.status = "ready";
      this.qr = null;
      this.retried = false;
      this.me = { number: client.info.wid.user, name: client.info.pushname ?? "" };
      if (!this.stored.first_ready_at) {
        this.stored.first_ready_at = new Date().toISOString();
        this.onChange();
      }
      log(this.stored.id, "ready", this.me.number);
    });
    client.on("auth_failure", (message: string) => {
      if (!current()) return;
      this.status = "failed";
      this.error = message;
      log(this.stored.id, "auth failure", message);
    });
    client.on("disconnected", (reason: string) => {
      if (!current()) return;
      this.status = "disconnected";
      this.error = String(reason);
      this.me = null;
      log(this.stored.id, "disconnected", reason);
    });
    try {
      await client.initialize();
    } catch (e) {
      if (!current()) return;
      this.status = "failed";
      this.error = e instanceof Error ? e.message : String(e);
      log(this.stored.id, "failed to start", this.error);
      // Once more on its own, a few seconds later, with the profile freed again.
      if (!this.retried) {
        this.retried = true;
        setTimeout(() => {
          if (current() && this.status === "failed") void this.start();
        }, 5000);
      }
    }
  }

  async stop() {
    const client = this.client;
    this.client = null;
    if (client) await client.destroy().catch(() => undefined);
  }

  view() {
    return {
      id: this.stored.id,
      name: this.stored.name,
      created_at: this.stored.created_at,
      first_ready_at: this.stored.first_ready_at,
      status: this.status,
      me: this.me,
      error: this.status === "ready" ? null : this.error,
    };
  }

  ready(): ClientType {
    if (this.status !== "ready" || !this.client) throw new NotReady("not_ready");
    return this.client;
  }
}

/** Ends any browser still running on an account's profile (a start that crashed leaves one)
 * and removes the profile's lock files, so the next start can open it. */
async function freeProfile(id: string) {
  const profile = path.join(AUTH, `session-${id}`);
  let killed = 0;
  for (const pid of await readdir("/proc").catch(() => [] as string[])) {
    if (!/^\d+$/.test(pid) || Number(pid) === process.pid) continue;
    const cmd = await readFile(`/proc/${pid}/cmdline`, "utf8").catch(() => "");
    if (cmd.includes("chrom") && cmd.includes(profile)) {
      try {
        process.kill(Number(pid), "SIGKILL");
        killed++;
      } catch {
        /* already gone */
      }
    }
  }
  if (killed) {
    log(id, "ended", killed, "leftover browser processes");
    await new Promise((r) => setTimeout(r, 500));
  }
  for (const lock of ["SingletonLock", "SingletonSocket", "SingletonCookie"]) {
    await rm(path.join(profile, lock), { force: true }).catch(() => undefined);
  }
}

function log(id: string, ...what: unknown[]) {
  console.log(JSON.stringify({ at: new Date().toISOString(), session: id, message: what.map(String).join(" ") }));
}

export class Sessions {
  private sessions = new Map<string, Session>();

  async load() {
    await mkdir(AUTH, { recursive: true });
    let stored: Stored[] = [];
    try {
      stored = JSON.parse(await readFile(LIST, "utf8")) as Stored[];
    } catch {
      /* first start */
    }
    for (const s of stored) {
      const session = new Session(s, () => void this.save());
      this.sessions.set(s.id, session);
      // One at a time: each starts a browser.
      await session.start();
    }
  }

  private async save() {
    const list = [...this.sessions.values()].map((s) => s.stored);
    const tmp = `${LIST}.tmp`;
    await writeFile(tmp, JSON.stringify(list, null, 2));
    await rename(tmp, LIST);
  }

  list() {
    return [...this.sessions.values()].map((s) => s.view());
  }

  get(id: string): Session {
    const s = this.sessions.get(id);
    if (!s) throw new NotFound("not_found");
    return s;
  }

  async create(name: string) {
    if (this.sessions.size >= MAX_SESSIONS) throw new NotAllowed("too_many_sessions");
    const stored: Stored = { id: randomUUID().slice(0, 8), name: name.slice(0, 60) || "חשבון", created_at: new Date().toISOString(), first_ready_at: null };
    const session = new Session(stored, () => void this.save());
    this.sessions.set(stored.id, session);
    await this.save();
    void session.start();
    return session.view();
  }

  /** Starts the account again; while it is already starting, a click changes nothing. */
  async restart(id: string) {
    const s = this.get(id);
    if (!s.busy) void s.start();
    return s.view();
  }

  /** Signs the account out of WhatsApp and forgets it, login files included. */
  async remove(id: string) {
    const s = this.get(id);
    try {
      if (s.client && s.status === "ready") await s.client.logout();
    } catch {
      /* already gone */
    }
    await s.stop();
    await freeProfile(id);
    this.sessions.delete(id);
    await this.save();
    await rm(path.join(AUTH, `session-${id}`), { recursive: true, force: true });
  }

  async qr(id: string) {
    const s = this.get(id);
    return { status: s.status, qr: s.qr ? await QRCode.toDataURL(s.qr, { margin: 1, width: 280 }) : null };
  }

  /** What the account may send to: the groups it is in and its saved contacts, nothing else. */
  async chats(id: string) {
    const client = this.get(id).ready();
    const groups = await myGroups(client);
    const contacts = (await client.getContacts())
      .filter((c) => c.isMyContact && c.isUser && !c.isGroup && c.id.server === "c.us" && !c.isMe)
      .map((c) => ({ id: c.id._serialized, name: c.name || c.pushname || c.number, number: c.number }));
    const byName = (a: { name: string }, b: { name: string }) => a.name.localeCompare(b.name, "he");
    return { groups: groups.sort(byName), contacts: contacts.sort(byName) };
  }

  /** Sends one message, as a person would: "typing…" first. Only to a group the account is in
   * or a saved contact. */
  async send(id: string, chatId: string, text: string, typingMs: number) {
    const client = this.get(id).ready();
    if (chatId.endsWith("@g.us")) {
      if (!(await myGroups(client, chatId)).length) throw new NotAllowed("not_a_member");
    } else if (chatId.endsWith("@c.us")) {
      const contact = await client.getContactById(chatId).catch(() => null);
      if (!contact?.isMyContact) throw new NotAllowed("not_a_contact");
    } else {
      throw new NotAllowed("not_a_contact");
    }
    // Not through client.getChatById: it builds the whole chat, last message included, and
    // that read fails in WhatsApp Web of September 2026.
    const state = (what: "typing" | "stop") =>
      page(client)
        .evaluate((s, c) => (window as any).WWebJS.sendChatstate(s, c), what, chatId)
        .catch(() => undefined);
    if (typingMs > 0) {
      await state("typing");
      await new Promise((r) => setTimeout(r, Math.min(typingMs, 5000)));
    }
    let message;
    try {
      message = await client.sendMessage(chatId, text, { sendSeen: false });
    } catch (e) {
      log(id, "send outcome unknown", e instanceof Error ? e.message : String(e));
      throw new OutcomeUnknown("outcome_unknown");
    }
    await state("stop");
    // Sent, even with no message back: WhatsApp Web of September 2026 stores the sent message
    // under another key than the one whatsapp-web.js looks it up by, so it hands back nothing.
    return { id: message?.id?._serialized ?? null };
  }
}

function page(client: ClientType): import("puppeteer-core").Page {
  return (client as unknown as { pupPage: import("puppeteer-core").Page }).pupPage;
}

/** The groups the account is a member of (or the one asked, if it is), read straight from
 * WhatsApp Web's own lists: whatsapp-web.js's getChats reads each chat's last message too, and
 * that read fails in WhatsApp Web of September 2026 ("No key or key range specified"). */
async function myGroups(client: ClientType, only?: string): Promise<{ id: string; name: string }[]> {
  return page(client).evaluate((only: string | null) => {
    const w = window as any;
    const Me = w.require("WAWebUserPrefsMeUser");
    const me = [Me.getMaybeMePnUser?.(), Me.getMaybeMeLidUser?.()].filter(Boolean).map((u: any) => u._serialized);
    return w
      .require("WAWebCollections")
      .Chat.getModelsArray()
      .filter((c: any) => c.id.server === "g.us" && c.groupMetadata && (!only || c.id._serialized === only))
      .filter((c: any) => c.groupMetadata.participants.getModelsArray().some((p: any) => me.includes(p.id._serialized)))
      .map((c: any) => ({ id: c.id._serialized as string, name: String(c.formattedTitle ?? c.name ?? "") }));
  }, only ?? null);
}
