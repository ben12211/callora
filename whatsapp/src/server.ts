// The internal API the Callora server calls. It listens on the Docker network only and
// every request carries WHATSAPP_TOKEN; pacing (delays, limits, quiet hours) is decided by
// the Callora server, which sends one message at a time.

import { timingSafeEqual } from "node:crypto";
import http from "node:http";
import { MAX_SESSIONS, NotAllowed, NotFound, NotReady, OutcomeUnknown, Sessions } from "./sessions.js";

const TOKEN = process.env.WHATSAPP_TOKEN ?? "";
const PORT = Number(process.env.PORT ?? 3100);
const sessions = new Sessions();

function authorized(req: http.IncomingMessage): boolean {
  const given = Buffer.from(String(req.headers["x-internal-token"] ?? ""));
  const want = Buffer.from(TOKEN);
  return TOKEN.length >= 16 && given.length === want.length && timingSafeEqual(given, want);
}

async function body(req: http.IncomingMessage): Promise<Record<string, unknown>> {
  // Whole bytes first: a Hebrew letter can be split between two chunks.
  const chunks: Buffer[] = [];
  let size = 0;
  for await (const chunk of req) {
    size += (chunk as Buffer).length;
    if (size > 64 * 1024) throw new Error("too_large");
    chunks.push(chunk as Buffer);
  }
  const raw = Buffer.concat(chunks).toString("utf8");
  return raw ? (JSON.parse(raw) as Record<string, unknown>) : {};
}

function send(res: http.ServerResponse, code: number, value?: unknown) {
  res.writeHead(code, { "content-type": "application/json" });
  res.end(value === undefined ? "" : JSON.stringify(value));
}

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url ?? "/", "http://internal");
  const method = req.method ?? "GET";
  if (url.pathname === "/health") return send(res, 200, { ok: true });
  if (!authorized(req)) return send(res, 401, { error: "unauthorized" });
  const parts = url.pathname.split("/").filter(Boolean);
  try {
    if (parts[0] !== "sessions") return send(res, 404, { error: "not_found" });
    const id = parts[1];
    const action = parts[2];
    if (!id && method === "GET") return send(res, 200, { sessions: sessions.list(), max: MAX_SESSIONS });
    if (!id && method === "POST") {
      const b = await body(req);
      return send(res, 201, await sessions.create(String(b.name ?? "")));
    }
    if (id && !action && method === "DELETE") {
      await sessions.remove(id);
      return send(res, 204);
    }
    if (action === "restart" && method === "POST") return send(res, 200, await sessions.restart(id));
    if (action === "qr" && method === "GET") return send(res, 200, await sessions.qr(id));
    if (action === "chats" && method === "GET") return send(res, 200, await sessions.chats(id));
    if (action === "send" && method === "POST") {
      const b = await body(req);
      const chatId = String(b.chat_id ?? "");
      const text = String(b.text ?? "");
      if (!text.trim()) return send(res, 400, { error: "empty" });
      return send(res, 200, await sessions.send(id, chatId, text.slice(0, 4000), Number(b.typing_ms ?? 0)));
    }
    return send(res, 404, { error: "not_found" });
  } catch (e) {
    if (e instanceof NotFound) return send(res, 404, { error: "not_found" });
    if (e instanceof NotReady) return send(res, 409, { error: "not_ready" });
    if (e instanceof NotAllowed) return send(res, 403, { error: e.message });
    if (e instanceof OutcomeUnknown) return send(res, 502, { error: "outcome_unknown" });
    console.error(JSON.stringify({ at: new Date().toISOString(), error: e instanceof Error ? e.message : String(e), path: url.pathname }));
    return send(res, 500, { error: "failed" });
  }
});

if (TOKEN.length < 16) console.error("WHATSAPP_TOKEN is missing or shorter than 16 characters: every request is refused");
server.listen(PORT, "0.0.0.0", () => console.log(JSON.stringify({ message: `whatsapp service on ${PORT}` })));
void sessions.load();

for (const signal of ["SIGTERM", "SIGINT"] as const) {
  process.on(signal, () => {
    server.close();
    process.exit(0);
  });
}

// whatsapp-web.js leaves some of its own promises unhandled (a browser closed while a page
// response is read, as when an account is removed): logged, not the end of every account.
process.on("unhandledRejection", (e) => {
  console.error(JSON.stringify({ at: new Date().toISOString(), unhandled: e instanceof Error ? e.message : String(e) }));
});
