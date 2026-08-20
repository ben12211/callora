import type {
  AdminUser,
  AgentConfig,
  AuditEvent,
  Business,
  CallRecord,
  CallTranscriptTurn,
} from '../../domain/models.js';
import type { CallPreview } from './call-preview.js';
import type { SettingGroup, SettingView } from '../../platform/settings.js';
import type { ProviderStatus } from '../../realtime/provider-catalog.js';
import { PROVIDER_CATALOG } from '../../realtime/provider-catalog.js';
import { REALTIME_PROVIDERS, type RealtimeProvider } from '../../realtime/provider.js';
import { badge, csrfField, escapeHtml, flash, formatDate } from './layout.js';

export function loginPage(options: { error?: string; email?: string }): string {
  return `<section class="login panel">
  <h1>Sign in</h1>
  <p class="lede">Callora control plane</p>
  ${flash('error', options.error)}
  <form method="post" action="/dashboard/login">
    <label for="email">Email</label>
    <input id="email" name="email" type="email" autocomplete="username" required value="${escapeHtml(
      options.email ?? '',
    )}" />
    <label for="password">Password</label>
    <input id="password" name="password" type="password" autocomplete="current-password" required />
    <div class="actions"><button type="submit">Sign in</button></div>
  </form>
</section>`;
}

export interface HomeData {
  businesses: Business[];
  agents: Map<string, AgentConfig>;
  callCount: number;
  recentCalls: CallRecord[];
  providers: ProviderStatus[];
  recentAudit: AuditEvent[];
  /** False for a business-scoped administrator, who owns no platform-wide surface. */
  platformScoped?: boolean;
}

export function homePage(data: HomeData): string {
  const activeBusinesses = data.businesses.filter((item) => item.active).length;
  const liveAgents = [...data.agents.values()].filter((agent) => agent.enabled).length;
  const configuredProviders = data.providers.filter((provider) => provider.configured);
  const platformScoped = data.platformScoped ?? true;

  // Provider credentials and the tenant list are the platform's, so a scoped
  // administrator is shown neither the panel nor a link that would answer 403.
  const providerPanel = platformScoped
    ? `<h2>Providers</h2>
<div class="panel">
  <p class="muted">${
    configuredProviders.length
      ? `Configured: ${configuredProviders.map((provider) => escapeHtml(provider.label)).join(', ')}.`
      : 'No execution provider has platform credentials yet. Calls answer with the static greeting.'
  }</p>
  <a class="button secondary" href="/dashboard/providers">Provider status</a>
</div>
`
    : '';
  const createBusiness = platformScoped
    ? `<div class="actions"><a class="button" href="/dashboard/businesses/new">Create business</a></div>`
    : '';

  return `<h1>Dashboard</h1>
<p class="lede">Callora is the source of truth for every business, agent, and provider choice below.</p>
<div class="grid">
  ${stat(String(data.businesses.length), 'Businesses')}
  ${stat(String(activeBusinesses), 'Active businesses')}
  ${stat(String(liveAgents), 'Enabled agents')}
  ${stat(String(data.callCount), 'Calls recorded')}
</div>

${providerPanel}
<h2>Businesses</h2>
${businessTable(data.businesses, data.agents)}
${createBusiness}

<h2>Recent calls</h2>
${callTable(data.recentCalls, data.businesses)}

<h2>Recent admin changes</h2>
${auditTable(data.recentAudit)}`;
}

function stat(value: string, label: string): string {
  return `<div class="panel stat"><div class="value">${escapeHtml(value)}</div><div class="label">${escapeHtml(
    label,
  )}</div></div>`;
}

export function businessListPage(
  businesses: Business[],
  agents: Map<string, AgentConfig>,
  notice?: string,
  platformScoped = true,
): string {
  // Creating a tenant is the platform administrator's job, so a scoped administrator is
  // not offered a button that would answer 403.
  const createBusiness = platformScoped
    ? `<div class="actions"><a class="button" href="/dashboard/businesses/new">Create business</a></div>`
    : '';
  return `<h1>Businesses</h1>
<p class="lede">Each business is a Callora tenant: its own number, greeting, agent, and provider.</p>
${flash('ok', notice)}
${businessTable(businesses, agents)}
${createBusiness}`;
}

function businessTable(businesses: Business[], agents: Map<string, AgentConfig>): string {
  if (businesses.length === 0) {
    return `<div class="panel"><p class="muted">No businesses yet.</p></div>`;
  }
  const rows = businesses
    .map((business) => {
      const agent = agents.get(business.id);
      return `<tr>
  <td><a href="/dashboard/businesses/${escapeHtml(business.id)}">${escapeHtml(business.name)}</a></td>
  <td class="mono">${escapeHtml(business.phoneNumber)}</td>
  <td>${badge(business.active, 'Active', 'Disabled')}</td>
  <td>${agent ? badge(agent.enabled, 'Agent on', 'Agent off') : '<span class="badge">No agent</span>'}</td>
  <td>${agent ? escapeHtml(PROVIDER_CATALOG[agent.voiceProvider].label) : '—'}</td>
  <td class="muted">${escapeHtml(formatDate(business.updatedAt))}</td>
</tr>`;
    })
    .join('');

  return `<div class="panel table-scroll"><table>
<thead><tr><th>Name</th><th>Number</th><th>Status</th><th>Agent</th><th>Provider</th><th>Updated</th></tr></thead>
<tbody>${rows}</tbody></table></div>`;
}

export function newBusinessPage(options: {
  csrfToken: string;
  error?: string;
  values?: { name?: string; phoneNumber?: string; greeting?: string };
}): string {
  const values = options.values ?? {};
  return `<h1>Create business</h1>
<p class="lede">The Twilio number is how Callora resolves the tenant on an incoming call, so it must be unique.</p>
${flash('error', options.error)}
<form class="panel" method="post" action="/dashboard/businesses">
  ${csrfField(options.csrfToken)}
  <label for="name">Business name</label>
  <input id="name" name="name" type="text" required maxlength="120" value="${escapeHtml(values.name ?? '')}" />
  <label for="phoneNumber">Twilio phone number
    <span class="hint">E.164, for example +15551234567.</span></label>
  <input id="phoneNumber" name="phoneNumber" type="text" required value="${escapeHtml(values.phoneNumber ?? '')}" />
  <label for="greeting">Fallback greeting
    <span class="hint">Spoken when the agent is off or its provider is unavailable.</span></label>
  <textarea id="greeting" name="greeting" required maxlength="500">${escapeHtml(values.greeting ?? '')}</textarea>
  <div class="checkbox">
    <input id="active" name="active" type="checkbox" value="on" checked />
    <label for="active" style="margin:0">Active</label>
  </div>
  <div class="actions">
    <button type="submit">Create business</button>
    <a class="button secondary" href="/dashboard/businesses">Cancel</a>
  </div>
</form>`;
}

export interface BusinessDetailData {
  business: Business;
  agent: AgentConfig | null;
  calls: CallRecord[];
  providers: ProviderStatus[];
  audit: AuditEvent[];
  csrfToken: string;
  notice?: string;
  error?: string;
}

/**
 * The `voice` and `model` fields mean something different on each provider, so only the
 * selected provider's note is shown. Rendering all of them at once was how a field that
 * one provider ignores could still read as if it applied.
 */
function providerHints(field: 'voice' | 'model', selected: RealtimeProvider): string {
  return REALTIME_PROVIDERS.map((id) => {
    const descriptor = PROVIDER_CATALOG[id];
    return `<p class="hint" data-hint="${field}" data-provider="${id}"${id === selected ? '' : ' hidden'}>${escapeHtml(
      field === 'voice' ? descriptor.voiceHint : descriptor.modelHint,
    )}</p>`;
  }).join('');
}

/** Swaps those notes when the provider changes, so the page never has to be reloaded. */
const PROVIDER_HINT_SCRIPT = `<script>
(function () {
  var select = document.getElementById('voiceProvider');
  if (!select) { return; }
  select.addEventListener('change', function () {
    var notes = document.querySelectorAll('[data-hint]');
    for (var index = 0; index < notes.length; index += 1) {
      notes[index].hidden = notes[index].getAttribute('data-provider') !== select.value;
    }
  });
})();
</script>`;

export function businessDetailPage(data: BusinessDetailData): string {
  const { business, agent, csrfToken } = data;
  const configured = new Set(data.providers.filter((provider) => provider.configured).map((p) => p.id));
  const selectedProvider: RealtimeProvider = agent?.voiceProvider ?? 'openai';

  return `<h1>${escapeHtml(business.name)} ${badge(business.active, 'Active', 'Disabled')}</h1>
<p class="lede mono">${escapeHtml(business.phoneNumber)} · ${escapeHtml(business.id)}</p>
${flash('ok', data.notice)}
${flash('error', data.error)}

<h2>Business details</h2>
<form class="panel" method="post" action="/dashboard/businesses/${escapeHtml(business.id)}">
  ${csrfField(csrfToken)}
  <label for="name">Business name</label>
  <input id="name" name="name" type="text" required maxlength="120" value="${escapeHtml(business.name)}" />
  <label for="phoneNumber">Twilio phone number</label>
  <input id="phoneNumber" name="phoneNumber" type="text" required value="${escapeHtml(business.phoneNumber)}" />
  <label for="greeting">Fallback greeting</label>
  <textarea id="greeting" name="greeting" required maxlength="500">${escapeHtml(business.greeting)}</textarea>
  <div class="checkbox">
    <input id="active" name="active" type="checkbox" value="on"${business.active ? ' checked' : ''} />
    <label for="active" style="margin:0">Active — a disabled business stops answering calls</label>
  </div>
  <div class="actions">
    <button type="submit">Save business</button>
    <a class="button secondary" href="/dashboard/businesses">Back</a>
  </div>
</form>

<h2>Agent configuration</h2>
<form class="panel" method="post" action="/dashboard/businesses/${escapeHtml(business.id)}/agent">
  ${csrfField(csrfToken)}
  <div class="checkbox">
    <input id="enabled" name="enabled" type="checkbox" value="on"${agent?.enabled ? ' checked' : ''} />
    <label for="enabled" style="margin:0">Agent enabled — answer calls with the realtime agent</label>
  </div>

  <label for="voiceProvider">Voice provider</label>
  <select id="voiceProvider" name="voiceProvider">
    ${REALTIME_PROVIDERS.map((id) => {
      const descriptor = PROVIDER_CATALOG[id];
      const available = configured.has(id);
      return `<option value="${id}"${id === selectedProvider ? ' selected' : ''}>${escapeHtml(
        descriptor.label,
      )}${available ? '' : ' — not configured on this platform'}</option>`;
    }).join('')}
  </select>
  <p class="muted">${REALTIME_PROVIDERS.map(
    (id) => `${escapeHtml(PROVIDER_CATALOG[id].label)}: ${escapeHtml(PROVIDER_CATALOG[id].summary)}`,
  ).join('<br />')}</p>

  <label for="language">Language
    <span class="hint">BCP-47 tag, for example he-IL or en-US. The agent always answers in it.</span></label>
  <input id="language" name="language" type="text" required value="${escapeHtml(agent?.language ?? 'he-IL')}" />

  <label for="agentGreeting">Greeting
    <span class="hint">The first thing the agent says when it picks up.</span></label>
  <textarea id="agentGreeting" name="greeting" required maxlength="500">${escapeHtml(agent?.greeting ?? '')}</textarea>

  <label for="instructions">System / custom instructions
    <span class="hint">Business context only. Callora's phone-agent policy is applied on top and cannot be overridden from here.</span></label>
  <textarea id="instructions" name="instructions" required maxlength="8000" style="min-height:12rem">${escapeHtml(
    agent?.instructions ?? '',
  )}</textarea>

  <label for="voice">Voice</label>
  ${providerHints('voice', selectedProvider)}
  <input id="voice" name="voice" type="text" maxlength="80" list="voice-suggestions" value="${escapeHtml(
    agent?.voice ?? '',
  )}" />
  <datalist id="voice-suggestions">${REALTIME_PROVIDERS.flatMap((id) =>
    PROVIDER_CATALOG[id].suggestedVoices.map((voice) => `<option value="${escapeHtml(voice)}"></option>`),
  ).join('')}</datalist>

  <label for="elevenLabsAgentId">ElevenLabs agent id
    <span class="hint">Only used when the provider is ElevenLabs. Give this business an agent of its own and
      Callora writes the configuration above into it on every save. Leave it empty to keep running on the shared
      platform agent, which is never written to.</span></label>
  <input id="elevenLabsAgentId" name="elevenLabsAgentId" type="text" maxlength="120" value="${escapeHtml(
    agent?.elevenLabsAgentId ?? '',
  )}" />

  <label for="realtimeModel">Model</label>
  ${providerHints('model', selectedProvider)}
  <input id="realtimeModel" name="realtimeModel" type="text" required maxlength="80" list="model-suggestions" value="${escapeHtml(
    agent?.realtimeModel ?? 'gpt-realtime-2.1',
  )}" />
  <datalist id="model-suggestions">${REALTIME_PROVIDERS.flatMap((id) =>
    PROVIDER_CATALOG[id].suggestedModels.map((model) => `<option value="${escapeHtml(model)}"></option>`),
  ).join('')}</datalist>

  <div class="actions">
    <button type="submit">Save agent configuration</button>
    <a class="button secondary" href="/dashboard/businesses/${escapeHtml(business.id)}/preview">See what a call will use</a>
  </div>
  ${PROVIDER_HINT_SCRIPT}
</form>

<h2>Recent calls</h2>
${callTable(data.calls, [business])}

<h2>Change history</h2>
${auditTable(data.audit)}`;
}

/**
 * Shows the resolved call configuration for one business.
 *
 * Rendered from the same builders the call path uses, so this page cannot drift from what
 * a caller actually reaches: if the prompt below is not the one in the form, the call is
 * not using the form either, and the warnings say why.
 */
export function callPreviewPage(options: {
  business: Business;
  preview: CallPreview;
  generatedAt: Date;
}): string {
  const { business, preview } = options;

  const rows = preview.fields
    .map(
      (field) => `<tr>
  <th>${escapeHtml(field.label)}</th>
  <td class="mono"${field.ignored ? ' style="color:var(--danger)"' : ''}>${escapeHtml(field.value)}${
    field.ignored ? ' — not sent' : ''
  }</td>
  <td class="muted">${escapeHtml(field.note ?? '')}</td>
</tr>`,
    )
    .join('');

  return `<h1>What the next call will use</h1>
<p class="lede">${escapeHtml(business.name)} · <span class="mono">${escapeHtml(business.phoneNumber)}</span></p>
<p class="muted">Resolved from the stored configuration by the same code that answers a call, at ${escapeHtml(
    formatDate(options.generatedAt),
  )}. If something here is not what you saved, the call is not reading what you saved either.</p>
${preview.warnings.map((warning) => flash('error', warning)).join('')}

<div class="panel">
  <h2 style="margin-top:0">${escapeHtml(preview.providerLabel)} ${badge(
    preview.providerConfigured,
    'Credentials present',
    'No platform credentials',
  )} ${badge(preview.enabled, 'Agent on', 'Agent off')}</h2>
  <div class="table-scroll"><table><tbody>${rows}</tbody></table></div>
</div>

<h2>First thing the caller hears</h2>
<div class="panel"><pre class="mono" style="white-space:pre-wrap;margin:0">${escapeHtml(preview.greeting)}</pre></div>

<h2>System prompt as the provider receives it</h2>
<div class="panel">
  <p class="muted">Your business instructions appear inside the configuration block. Everything around them is the
    Callora phone-agent policy, which is applied on top and outranks them: it can be narrowed, never widened.</p>
  <pre class="mono" style="white-space:pre-wrap;max-height:32rem;overflow:auto;margin:0">${escapeHtml(
    preview.instructions,
  )}</pre>
</div>
${
  preview.payload
    ? `<h2>Initiation payload</h2>
<div class="panel">
  <p class="muted">${escapeHtml(preview.payloadNote ?? '')}</p>
  <pre class="mono" style="white-space:pre-wrap;max-height:24rem;overflow:auto;margin:0">${escapeHtml(
    preview.payload,
  )}</pre>
</div>`
    : ''
}
<div class="actions"><a class="button secondary" href="/dashboard/businesses/${escapeHtml(
    business.id,
  )}">Back to the business</a></div>`;
}

export function callListPage(options: {
  calls: CallRecord[];
  businesses: Business[];
  businessId?: string;
  limit: number;
  offset: number;
}): string {
  const filter = `<form class="panel" method="get" action="/dashboard/calls">
  <label for="businessId">Business</label>
  <select id="businessId" name="businessId" onchange="this.form.submit()">
    <option value="">All businesses</option>
    ${options.businesses
      .map(
        (business) =>
          `<option value="${escapeHtml(business.id)}"${
            business.id === options.businessId ? ' selected' : ''
          }>${escapeHtml(business.name)}</option>`,
      )
      .join('')}
  </select>
  <div class="actions"><button class="secondary" type="submit">Apply</button></div>
</form>`;

  const query = (offset: number): string => {
    const params = new URLSearchParams();
    if (options.businessId) {
      params.set('businessId', options.businessId);
    }
    params.set('offset', String(offset));
    return `/dashboard/calls?${params.toString()}`;
  };

  const pager = `<div class="actions">
  ${
    options.offset > 0
      ? `<a class="button secondary" href="${query(Math.max(0, options.offset - options.limit))}">Previous</a>`
      : ''
  }
  ${
    options.calls.length === options.limit
      ? `<a class="button secondary" href="${query(options.offset + options.limit)}">Next</a>`
      : ''
  }
</div>`;

  return `<h1>Calls</h1>
<p class="lede">Every call Twilio has reported, newest first.</p>
${filter}
${callTable(options.calls, options.businesses)}
${pager}`;
}

function callTable(calls: CallRecord[], businesses: Business[]): string {
  if (calls.length === 0) {
    return `<div class="panel"><p class="muted">No calls recorded yet.</p></div>`;
  }
  const names = new Map(businesses.map((business) => [business.id, business.name]));
  const rows = calls
    .map(
      (call) => `<tr>
  <td><a href="/dashboard/calls/${escapeHtml(call.id)}">${escapeHtml(formatDate(call.startedAt))}</a></td>
  <td>${escapeHtml(names.get(call.businessId) ?? call.businessId)}</td>
  <td class="mono">${escapeHtml(call.fromNumber ?? 'unknown')}</td>
  <td class="mono">${escapeHtml(call.toNumber)}</td>
  <td>${escapeHtml(call.status)}</td>
  <td>${call.durationSeconds === null ? '—' : `${escapeHtml(call.durationSeconds)}s`}</td>
</tr>`,
    )
    .join('');

  return `<div class="panel table-scroll"><table>
<thead><tr><th>Started</th><th>Business</th><th>From</th><th>To</th><th>Status</th><th>Duration</th></tr></thead>
<tbody>${rows}</tbody></table></div>`;
}

export function callDetailPage(
  call: CallRecord,
  business: Business | null,
  transcript: CallTranscriptTurn[] = [],
): string {
  const rows: [string, string][] = [
    ['Call id', call.id],
    ['Business', business ? business.name : call.businessId],
    ['Twilio CallSid', call.twilioCallSid],
    ['Twilio StreamSid', call.twilioStreamSid ?? '—'],
    ['Provider', call.provider ?? '—'],
    ['Provider session id', call.providerSessionId ?? '—'],
    ['From', call.fromNumber ?? 'unknown'],
    ['To', call.toNumber],
    ['Status', call.status],
    ['Direction', call.direction ?? '—'],
    ['Duration', call.durationSeconds === null ? '—' : `${call.durationSeconds}s`],
    ['Started', formatDate(call.startedAt)],
    ['Ended', formatDate(call.endedAt)],
  ];

  return `<h1>Call detail</h1>
<p class="lede">${
    business
      ? `<a href="/dashboard/businesses/${escapeHtml(business.id)}">${escapeHtml(business.name)}</a>`
      : escapeHtml(call.businessId)
  }</p>
<div class="panel table-scroll"><table><tbody>${rows
    .map(([label, value]) => `<tr><th>${escapeHtml(label)}</th><td class="mono">${escapeHtml(value)}</td></tr>`)
    .join('')}</tbody></table></div>
${renderTranscript(transcript)}
<div class="actions"><a class="button secondary" href="/dashboard/calls">Back to calls</a></div>`;
}

/**
 * The conversation itself. Transcripts used to exist only as log lines, so the people who
 * run a business could never actually read what their agent said.
 */
function renderTranscript(transcript: CallTranscriptTurn[]): string {
  if (transcript.length === 0) {
    return `<h2>Transcript</h2>
<div class="panel"><p class="lede">No transcript was recorded for this call.</p></div>`;
  }

  const rows = transcript
    .map(
      (turn) => `<tr>
  <td>${escapeHtml(turn.speaker === 'caller' ? 'Caller' : 'Agent')}</td>
  <td>${escapeHtml(turn.content)}</td>
</tr>`,
    )
    .join('');

  return `<h2>Transcript</h2>
<div class="panel table-scroll"><table>
<thead><tr><th>Speaker</th><th>Said</th></tr></thead>
<tbody>${rows}</tbody></table></div>`;
}

export interface ProviderPageData {
  providers: ProviderStatus[];
  platformDefault: RealtimeProvider;
  settings: SettingView[];
  /** Revision the page was rendered from; submitted back to detect a concurrent change. */
  revision: string;
  /** False when the deployment has no SECRETS_KEY, so credentials cannot be stored. */
  secretsEditable: boolean;
  csrfToken: string;
  notice?: string;
  error?: string;
}

/** Where the value in force came from, so an operator can see what a save would replace. */
function sourceNote(setting: SettingView): string {
  if (setting.unreadable) {
    return 'A value is stored but cannot be decrypted with this SECRETS_KEY. Clear it and enter it again.';
  }
  switch (setting.source) {
    case 'callora':
      return setting.secret ? 'Stored in Callora.' : 'Saved here, overriding the environment.';
    case 'environment':
      return setting.secret
        ? 'Supplied by the environment. Entering a value here stores it in Callora instead.'
        : 'From the environment. Clear the field to go back to it.';
    default:
      return 'Not set; the built-in default applies.';
  }
}

function settingField(setting: SettingView, secretsEditable: boolean): string {
  const id = `setting-${setting.key}`;
  const label = `<label for="${id}">${escapeHtml(setting.label)}
    <span class="hint mono">${escapeHtml(setting.key)}</span>
    <span class="hint">${escapeHtml(setting.hint)}</span></label>`;

  if (setting.choices) {
    return `${label}
  <select id="${id}" name="${escapeHtml(setting.key)}">
    ${setting.choices
      .map(
        (choice) =>
          `<option value="${escapeHtml(choice)}"${choice === setting.value ? ' selected' : ''}>${escapeHtml(
            PROVIDER_CATALOG[choice as RealtimeProvider]?.label ?? choice,
          )}</option>`,
      )
      .join('')}
  </select>
  <p class="muted">${escapeHtml(sourceNote(setting))}</p>`;
  }

  if (setting.secret) {
    // The stored value is never rendered, so the field is always empty and blank means
    // "leave it as it is". Removing one is an explicit choice, hence the checkbox.
    const status = setting.configured
      ? badge(!setting.unreadable, 'Configured', 'Unreadable')
      : '<span class="badge">Not set</span>';
    return `${label}
  <p>${status} <span class="muted">${escapeHtml(sourceNote(setting))}</span></p>
  <input id="${id}" name="${escapeHtml(setting.key)}" type="password" autocomplete="new-password"
    placeholder="${setting.configured ? 'Leave blank to keep the current value' : 'Not set'}"${
      secretsEditable ? '' : ' disabled'
    } />
  ${
    setting.source === 'callora' || setting.unreadable
      ? `<div class="checkbox">
    <input id="clear-${escapeHtml(setting.key)}" name="clear" type="checkbox" value="${escapeHtml(setting.key)}" />
    <label for="clear-${escapeHtml(setting.key)}" style="margin:0">Remove the stored value and fall back to the environment</label>
  </div>`
      : ''
  }`;
  }

  if (setting.multiline) {
    return `${label}
  <textarea id="${id}" name="${escapeHtml(setting.key)}" rows="3">${escapeHtml(setting.value)}</textarea>
  <p class="muted">${escapeHtml(sourceNote(setting))}</p>`;
  }

  return `${label}
  <input id="${id}" name="${escapeHtml(setting.key)}" type="text" value="${escapeHtml(setting.value)}" />
  <p class="muted">${escapeHtml(sourceNote(setting))}</p>`;
}

function settingsForm(options: {
  group: SettingGroup;
  title: string;
  settings: SettingView[];
  secretsEditable: boolean;
  csrfToken: string;
  revision: string;
}): string {
  const fields = options.settings.filter((setting) => setting.group === options.group);
  if (fields.length === 0) {
    return '';
  }
  return `<form method="post" action="/dashboard/providers">
  ${csrfField(options.csrfToken)}
  <input type="hidden" name="revision" value="${escapeHtml(options.revision)}" />
  ${fields.map((setting) => settingField(setting, options.secretsEditable)).join('\n')}
  <div class="actions"><button type="submit">${escapeHtml(options.title)}</button></div>
</form>`;
}

export function providerPage(data: ProviderPageData): string {
  const cards = data.providers
    .map(
      (provider) => `<div class="panel">
  <h2 style="margin-top:0">${escapeHtml(provider.label)} ${badge(
    provider.configured,
    'Configured',
    'Not configured',
  )}${provider.isPlatformDefault ? ' <span class="badge">Default for new agents</span>' : ''}</h2>
  <p class="muted">${escapeHtml(provider.summary)}</p>
  <p><strong>Voice field:</strong> ${escapeHtml(provider.voiceHint)}<br />
     <strong>Model field:</strong> ${escapeHtml(provider.modelHint)}</p>
  ${
    provider.configured
      ? ''
      : `<p class="muted">Still missing: <span class="mono">${escapeHtml(
          provider.missingEnvironment.join(', ') || provider.requiredEnvironment.join(', '),
        )}</span></p>`
  }
  ${settingsForm({
    group: provider.id,
    title: `Save ${provider.label} settings`,
    settings: data.settings,
    secretsEditable: data.secretsEditable,
    csrfToken: data.csrfToken,
    revision: data.revision,
  })}
</div>`,
    )
    .join('');

  return `<h1>Providers</h1>
<p class="lede">Execution providers Callora can hand a call to, configured here rather than in the deployment's environment. A saved value overrides the environment variable of the same name and applies to the next call; clearing a field falls back to it.</p>
${flash('ok', data.notice)}
${flash('error', data.error)}
${
  data.secretsEditable
    ? ''
    : flash(
        'error',
        'This deployment has no SECRETS_KEY, so API keys cannot be stored safely and their fields are disabled. Set SECRETS_KEY in the environment to manage credentials here; everything else is still editable.',
      )
}

<h2>Platform</h2>
<div class="panel">
  ${settingsForm({
    group: 'platform',
    title: 'Save platform settings',
    settings: data.settings,
    secretsEditable: data.secretsEditable,
    csrfToken: data.csrfToken,
    revision: data.revision,
  })}
</div>

<h2>Execution providers</h2>
${cards}
<p class="muted">Stored credentials are encrypted and are never displayed again, here or anywhere else in Callora. The default for new agents is ${escapeHtml(
    PROVIDER_CATALOG[data.platformDefault].label,
  )}.</p>`;
}

export function auditPage(events: AuditEvent[], limit: number, offset: number): string {
  const query = (next: number): string => `/dashboard/audit?offset=${next}`;
  return `<h1>Audit history</h1>
<p class="lede">Administrative changes to businesses, agents, and accounts.</p>
${auditTable(events)}
<div class="actions">
  ${offset > 0 ? `<a class="button secondary" href="${query(Math.max(0, offset - limit))}">Previous</a>` : ''}
  ${events.length === limit ? `<a class="button secondary" href="${query(offset + limit)}">Next</a>` : ''}
</div>`;
}

function auditTable(events: AuditEvent[]): string {
  if (events.length === 0) {
    return `<div class="panel"><p class="muted">Nothing recorded yet.</p></div>`;
  }
  const rows = events
    .map(
      (event) => `<tr>
  <td class="muted">${escapeHtml(formatDate(event.createdAt))}</td>
  <td>${escapeHtml(event.actorLabel)}</td>
  <td class="mono">${escapeHtml(event.action)}</td>
  <td>${escapeHtml(event.summary)}</td>
  <td class="mono muted">${escapeHtml(summariseDetails(event.details))}</td>
</tr>`,
    )
    .join('');
  return `<div class="panel table-scroll"><table>
<thead><tr><th>When</th><th>Actor</th><th>Action</th><th>Summary</th><th>Changed</th></tr></thead>
<tbody>${rows}</tbody></table></div>`;
}

/** Field names only: instructions and greetings can be long, and the summary already says what happened. */
function summariseDetails(details: Record<string, unknown>): string {
  const changes = details['changes'];
  if (changes && typeof changes === 'object') {
    const keys = Object.keys(changes as Record<string, unknown>);
    return keys.length ? keys.join(', ') : 'no changes';
  }
  return Object.keys(details).join(', ');
}

export function settingsPage(options: {
  user: AdminUser;
  admins: AdminUser[];
  providers: ProviderStatus[];
  publicBaseUrl: string;
  platformDefault: RealtimeProvider;
  sessionTtlHours: number;
  apiKeyConfigured: boolean;
  csrfToken: string;
  notice?: string;
  error?: string;
}): string {
  const platformRows: [string, string][] = [
    ['Public base URL', options.publicBaseUrl],
    ['Voice webhook', `${options.publicBaseUrl}/webhooks/twilio/voice`],
    ['Status callback', `${options.publicBaseUrl}/webhooks/twilio/call-status`],
    ['Media stream', `${options.publicBaseUrl.replace(/^http/, 'ws')}/webhooks/twilio/media`],
    ['Default provider for new agents', PROVIDER_CATALOG[options.platformDefault].label],
    [
      'Configured providers',
      options.providers
        .filter((provider) => provider.configured)
        .map((provider) => provider.label)
        .join(', ') || 'none',
    ],
    ['Session lifetime', `${options.sessionTtlHours} hours`],
    ['Management API key', options.apiKeyConfigured ? 'configured' : 'not configured'],
  ];

  return `<h1>Settings</h1>
<p class="lede">Your account and this deployment's platform configuration.</p>
${flash('ok', options.notice)}
${flash('error', options.error)}

<h2>Account</h2>
<div class="panel">
  <table><tbody>
    <tr><th>Name</th><td>${escapeHtml(options.user.name)}</td></tr>
    <tr><th>Email</th><td class="mono">${escapeHtml(options.user.email)}</td></tr>
    <tr><th>Last sign-in</th><td>${escapeHtml(formatDate(options.user.lastLoginAt))}</td></tr>
  </tbody></table>
</div>

<h2>Change password</h2>
<form class="panel" method="post" action="/dashboard/settings/password">
  ${csrfField(options.csrfToken)}
  <label for="currentPassword">Current password</label>
  <input id="currentPassword" name="currentPassword" type="password" autocomplete="current-password" required />
  <label for="newPassword">New password <span class="hint">At least 12 characters.</span></label>
  <input id="newPassword" name="newPassword" type="password" autocomplete="new-password" required />
  <label for="confirmPassword">Confirm new password</label>
  <input id="confirmPassword" name="confirmPassword" type="password" autocomplete="new-password" required />
  <div class="actions"><button type="submit">Change password</button></div>
  <p class="muted">Changing your password signs out every other session.</p>
</form>

<h2>Platform</h2>
<div class="panel table-scroll"><table><tbody>${platformRows
    .map(([label, value]) => `<tr><th>${escapeHtml(label)}</th><td class="mono">${escapeHtml(value)}</td></tr>`)
    .join('')}</tbody></table>
  <p class="muted">Provider credentials and the default provider are managed on the
    <a href="/dashboard/providers">Providers</a> page. Everything above is fixed for the lifetime of the process.</p>
</div>

<h2>Administrators</h2>
<div class="panel table-scroll"><table>
<thead><tr><th>Name</th><th>Email</th><th>Status</th><th>Last sign-in</th></tr></thead>
<tbody>${options.admins
    .map(
      (admin) => `<tr><td>${escapeHtml(admin.name)}</td><td class="mono">${escapeHtml(
        admin.email,
      )}</td><td>${badge(admin.active, 'Active', 'Disabled')}</td><td class="muted">${escapeHtml(
        formatDate(admin.lastLoginAt),
      )}</td></tr>`,
    )
    .join('')}</tbody></table></div>`;
}

export function notFoundPage(message: string): string {
  return `<h1>Not found</h1><div class="panel"><p class="muted">${escapeHtml(message)}</p></div>
<div class="actions"><a class="button secondary" href="/dashboard">Back to the dashboard</a></div>`;
}
