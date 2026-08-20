/**
 * Server-rendered dashboard shell.
 *
 * The control plane ships as plain HTML from the same Fastify service: no bundler, no
 * client framework, and no second container to run. Every page is a form post, so the
 * dashboard works with the session cookie alone and needs no token in the browser.
 */

export function escapeHtml(value: unknown): string {
  return String(value ?? '')
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;')
    .replaceAll('"', '&quot;')
    .replaceAll("'", '&#39;');
}

export interface NavigationItem {
  href: string;
  label: string;
  /** Hidden from a business-scoped administrator, who cannot open it. */
  platformOnly?: boolean;
}

export const NAVIGATION: readonly NavigationItem[] = [
  { href: '/dashboard', label: 'Home' },
  { href: '/dashboard/businesses', label: 'Businesses' },
  { href: '/dashboard/calls', label: 'Calls' },
  { href: '/dashboard/providers', label: 'Providers', platformOnly: true },
  { href: '/dashboard/audit', label: 'Audit history' },
  { href: '/dashboard/settings', label: 'Settings' },
];

/** Offering a link that answers 403 is worse than not offering it. */
export function navigationFor(role: 'platform' | 'business'): readonly NavigationItem[] {
  return role === 'platform' ? NAVIGATION : NAVIGATION.filter((item) => !item.platformOnly);
}

const STYLES = `
:root {
  color-scheme: light dark;
  --bg: #f6f7f9;
  --panel: #ffffff;
  --border: #d9dde3;
  --text: #1c2430;
  --muted: #5c6673;
  --accent: #2f5bd0;
  --danger: #b3261e;
  --ok: #14733f;
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg: #14171c;
    --panel: #1c2028;
    --border: #313742;
    --text: #eef1f5;
    --muted: #a3acba;
    --accent: #7ea2ff;
    --danger: #ff8b82;
    --ok: #5bd394;
  }
}
* { box-sizing: border-box; }
body {
  margin: 0;
  background: var(--bg);
  color: var(--text);
  font: 15px/1.5 system-ui, -apple-system, "Segoe UI", sans-serif;
}
a { color: var(--accent); }
header.top {
  display: flex; align-items: center; justify-content: space-between; gap: 1rem;
  padding: 0.75rem 1.25rem; background: var(--panel); border-bottom: 1px solid var(--border);
  flex-wrap: wrap;
}
.brand { font-weight: 700; letter-spacing: 0.02em; }
nav.top a {
  display: inline-block; padding: 0.35rem 0.6rem; border-radius: 6px;
  text-decoration: none; color: var(--text);
}
nav.top a[aria-current="page"] { background: var(--accent); color: #fff; }
main { max-width: 1080px; margin: 0 auto; padding: 1.5rem 1.25rem 4rem; }
h1 { font-size: 1.4rem; margin: 0 0 0.25rem; }
h2 { font-size: 1.1rem; margin: 1.75rem 0 0.5rem; }
p.lede { color: var(--muted); margin: 0 0 1.25rem; }
.panel {
  background: var(--panel); border: 1px solid var(--border); border-radius: 10px;
  padding: 1.1rem 1.2rem; margin-bottom: 1.1rem;
}
.grid { display: grid; gap: 1rem; grid-template-columns: repeat(auto-fit, minmax(210px, 1fr)); }
.stat .value { font-size: 1.9rem; font-weight: 700; }
.stat .label { color: var(--muted); font-size: 0.85rem; text-transform: uppercase; letter-spacing: 0.04em; }
table { width: 100%; border-collapse: collapse; }
th, td { text-align: left; padding: 0.55rem 0.6rem; border-bottom: 1px solid var(--border); vertical-align: top; }
th { font-size: 0.78rem; text-transform: uppercase; letter-spacing: 0.04em; color: var(--muted); }
.table-scroll { overflow-x: auto; }
label { display: block; margin: 0.85rem 0 0.25rem; font-weight: 600; font-size: 0.9rem; }
label .hint { display: block; font-weight: 400; color: var(--muted); font-size: 0.82rem; }
input[type="text"], input[type="email"], input[type="password"], select, textarea {
  width: 100%; padding: 0.5rem 0.6rem; border: 1px solid var(--border); border-radius: 7px;
  background: var(--bg); color: var(--text); font: inherit;
}
textarea { min-height: 7rem; resize: vertical; }
.checkbox { display: flex; align-items: center; gap: 0.5rem; margin-top: 0.9rem; }
.checkbox input { width: auto; }
button, .button {
  display: inline-block; padding: 0.5rem 0.95rem; border-radius: 7px; border: 1px solid transparent;
  background: var(--accent); color: #fff; font: inherit; font-weight: 600; cursor: pointer;
  text-decoration: none;
}
button.secondary, .button.secondary { background: transparent; border-color: var(--border); color: var(--text); }
button.danger { background: var(--danger); }
.actions { display: flex; gap: 0.6rem; align-items: center; flex-wrap: wrap; margin-top: 1.1rem; }
.badge {
  display: inline-block; padding: 0.12rem 0.5rem; border-radius: 999px; font-size: 0.78rem;
  border: 1px solid var(--border); color: var(--muted);
}
.badge.ok { color: var(--ok); border-color: var(--ok); }
.badge.off { color: var(--danger); border-color: var(--danger); }
.flash { padding: 0.7rem 0.9rem; border-radius: 8px; margin-bottom: 1rem; border: 1px solid; }
.flash.error { border-color: var(--danger); color: var(--danger); }
.flash.ok { border-color: var(--ok); color: var(--ok); }
.mono { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 0.85rem; }
.muted { color: var(--muted); }
.login { max-width: 380px; margin: 12vh auto; }
.inline-form { display: inline; }
`;

export interface PageOptions {
  title: string;
  currentPath: string;
  /** Absent on the login page, which renders without navigation. */
  userLabel?: string;
  csrfToken?: string;
  /** Decides which navigation entries this administrator is offered. */
  role?: 'platform' | 'business';
  body: string;
}

export function renderPage(options: PageOptions): string {
  const nav = options.userLabel
    ? `<header class="top">
  <span class="brand">Callora Control Plane</span>
  <nav class="top">${navigationFor(options.role ?? 'platform').map(
    (item) =>
      `<a href="${item.href}"${isCurrent(item.href, options.currentPath) ? ' aria-current="page"' : ''}>${escapeHtml(
        item.label,
      )}</a>`,
  ).join('')}</nav>
  <form method="post" action="/dashboard/logout" class="inline-form">
    <input type="hidden" name="_csrf" value="${escapeHtml(options.csrfToken)}" />
    <span class="muted">${escapeHtml(options.userLabel)}</span>
    <button class="secondary" type="submit">Sign out</button>
  </form>
</header>`
    : '';

  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<title>${escapeHtml(options.title)} · Callora</title>
<style>${STYLES}</style>
</head>
<body>
${nav}
<main>${options.body}</main>
</body>
</html>`;
}

function isCurrent(href: string, currentPath: string): boolean {
  return href === '/dashboard' ? currentPath === '/dashboard' : currentPath.startsWith(href);
}

export function flash(kind: 'ok' | 'error', message: string | undefined): string {
  return message ? `<div class="flash ${kind}">${escapeHtml(message)}</div>` : '';
}

export function badge(active: boolean, onLabel: string, offLabel: string): string {
  return `<span class="badge ${active ? 'ok' : 'off'}">${escapeHtml(active ? onLabel : offLabel)}</span>`;
}

export function formatDate(value: Date | string | null | undefined): string {
  if (!value) {
    return '—';
  }
  const date = value instanceof Date ? value : new Date(value);
  return Number.isNaN(date.getTime()) ? '—' : date.toISOString().replace('T', ' ').slice(0, 19) + 'Z';
}

export function csrfField(token: string): string {
  return `<input type="hidden" name="_csrf" value="${escapeHtml(token)}" />`;
}
