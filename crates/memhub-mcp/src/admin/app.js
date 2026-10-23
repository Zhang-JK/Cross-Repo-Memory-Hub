'use strict';

// ---------------------------------------------------------------------------
// DOM helpers
// ---------------------------------------------------------------------------

const $ = (sel, root = document) => root.querySelector(sel);

/** Builds an element. Strings become text nodes, so content is never parsed as HTML. */
function h(tag, props, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(props || {})) {
    if (v === null || v === undefined || v === false) continue;
    if (k === 'class') el.className = v;
    else if (k === 'text') el.textContent = v;
    else if (k === 'on') for (const [ev, fn] of Object.entries(v)) el.addEventListener(ev, fn);
    else if (k === 'dataset') Object.assign(el.dataset, v);
    else if (k in el && typeof v !== 'string') el[k] = v;
    else el.setAttribute(k, v === true ? '' : v);
  }
  append(el, children);
  return el;
}

function append(el, children) {
  for (const c of children.flat(Infinity)) {
    if (c === null || c === undefined || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}

const SVG_NS = 'http://www.w3.org/2000/svg';
function s(tag, attrs, ...children) {
  const el = document.createElementNS(SVG_NS, tag);
  for (const [k, v] of Object.entries(attrs || {})) el.setAttribute(k, v);
  for (const c of children.flat()) {
    if (c !== null && c !== undefined) el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}

function esc(text) {
  return String(text).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
}

function debounce(fn, ms) {
  let t;
  return (...args) => { clearTimeout(t); t = setTimeout(() => fn(...args), ms); };
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

const pad = (n) => String(n).padStart(2, '0');

function fmtTime(iso) {
  if (!iso) return '—';
  const d = new Date(iso);
  if (isNaN(d)) return iso;
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}

function ago(iso) {
  if (!iso) return '';
  const secs = (Date.now() - new Date(iso).getTime()) / 1000;
  if (secs < 45) return 'just now';
  if (secs < 3600) return `${Math.round(secs / 60)}m ago`;
  if (secs < 86400) return `${Math.round(secs / 3600)}h ago`;
  if (secs < 86400 * 30) return `${Math.round(secs / 86400)}d ago`;
  return fmtTime(iso).slice(0, 10);
}

function fmtDuration(secs) {
  if (secs === null || secs === undefined || isNaN(secs)) return '—';
  secs = Math.max(0, secs);
  if (secs < 1) return '<1s';
  if (secs < 60) return `${Math.round(secs)}s`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m ${Math.round(secs % 60)}s`;
  if (secs < 86400) return `${Math.floor(secs / 3600)}h ${Math.floor((secs % 3600) / 60)}m`;
  return `${Math.floor(secs / 86400)}d ${Math.floor((secs % 86400) / 3600)}h`;
}

const shortId = (id) => (id || '').replace(/^admin-/, 'admin·').slice(0, 8);
const timeCell = (iso) => h('td', { class: 'nowrap', title: iso || '' }, fmtTime(iso));

function agentLabel(name) {
  if (!name) return h('span', { class: 'muted', title: 'Connected but never identified with connect_agent' }, 'anonymous');
  if (name === '(admin)') return h('span', { class: 'admin-actor', title: 'Change made from this admin UI' }, 'admin');
  return name;
}

function actionOf(r) {
  return r.event === 'memory_changed' ? r.action : r.event;
}

const ACTION_LABELS = {
  add: 'add', update: 'update', delete: 'delete', group_add: 'group +', group_remove: 'group −',
  folder_created: 'folder +', folder_deleted: 'folder −',
};

function memoryHash(id) {
  const [scope, owner, slug] = (id || '').split('/');
  return '#/memories?' + new URLSearchParams({ scope, owner, id: `${scope}/${owner}/${slug}` });
}

// ---------------------------------------------------------------------------
// Tiny Markdown renderer: everything is escaped first, then a fixed set of
// tags is generated, so memory content can never inject markup.
// ---------------------------------------------------------------------------

function mdInline(text) {
  return text.split(/(`[^`]+`)/).map((part, i) => {
    if (i % 2) return `<code>${esc(part.slice(1, -1))}</code>`;
    return esc(part)
      .replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>')
      .replace(/(^|[^*\w])\*([^*\s][^*]*?)\*(?!\w)/g, '$1<em>$2</em>')
      .replace(/\[([^\]]+)\]\((https?:\/\/[^\s)]+)\)/g, '<a href="$2" target="_blank" rel="noopener noreferrer">$1</a>');
  }).join('');
}

function renderMarkdown(src) {
  const lines = src.replace(/\r\n/g, '\n').split('\n');
  const out = [];
  let para = [];
  let list = null;
  const flushPara = () => { if (para.length) { out.push(`<p>${mdInline(para.join(' '))}</p>`); para = []; } };
  const flushList = () => { if (list) { out.push(`<${list.tag}>${list.items.map((i) => `<li>${mdInline(i)}</li>`).join('')}</${list.tag}>`); list = null; } };
  const flush = () => { flushPara(); flushList(); };

  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const fence = line.match(/^\s*(```|~~~)/);
    if (fence) {
      flush();
      const code = [];
      for (i++; i < lines.length && !lines[i].trim().startsWith(fence[1]); i++) code.push(lines[i]);
      out.push(`<pre><code>${esc(code.join('\n'))}</code></pre>`);
      continue;
    }
    let m;
    if (!line.trim()) { flush(); continue; }
    if ((m = line.match(/^(#{1,4})\s+(.*)$/))) { flush(); out.push(`<h${m[1].length}>${mdInline(m[2])}</h${m[1].length}>`); continue; }
    if (/^\s*([-*_])(\s*\1){2,}\s*$/.test(line)) { flush(); out.push('<hr>'); continue; }
    if ((m = line.match(/^>\s?(.*)$/))) { flush(); out.push(`<blockquote>${mdInline(m[1])}</blockquote>`); continue; }
    if ((m = line.match(/^\s*([-*+]|\d+[.)])\s+(.*)$/))) {
      flushPara();
      const tag = /\d/.test(m[1]) ? 'ol' : 'ul';
      if (!list || list.tag !== tag) { flushList(); list = { tag, items: [] }; }
      list.items.push(m[2]);
      continue;
    }
    if (list && /^\s{2,}\S/.test(line)) { list.items[list.items.length - 1] += ' ' + line.trim(); continue; }
    flushList();
    para.push(line.trim());
  }
  flush();
  return out.join('\n');
}

// ---------------------------------------------------------------------------
// API
// ---------------------------------------------------------------------------

const TOKEN_KEY = 'memhub-admin-token';
const app = {
  token: localStorage.getItem(TOKEN_KEY) || '',
  timers: [],
  server: null,
};

class ApiError extends Error {
  constructor(message, status) { super(message); this.status = status; }
}

async function api(path, opts = {}) {
  const headers = { accept: 'application/json' };
  if (app.token) headers.authorization = `Bearer ${app.token}`;
  const init = { method: opts.method || 'GET', headers };
  if (opts.body !== undefined) {
    headers['content-type'] = 'application/json';
    init.body = JSON.stringify(opts.body);
  }
  const res = await fetch('/api/admin' + path, init);
  let data = null;
  try { data = await res.json(); } catch (_) { /* empty body */ }
  if (res.status === 401) {
    showLogin('The admin token is missing or wrong.');
    throw new ApiError('Admin token required', 401);
  }
  if (!res.ok) throw new ApiError((data && data.message) || `${res.status} ${res.statusText}`, res.status);
  return data;
}

function query(params) {
  const q = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) if (v !== undefined && v !== null && v !== '') q.set(k, v);
  const str = q.toString();
  return str ? `?${str}` : '';
}

// ---------------------------------------------------------------------------
// Toasts and modals
// ---------------------------------------------------------------------------

function toast(message, kind) {
  const el = h('div', { class: `toast ${kind || ''}`, text: message });
  $('#toasts').append(el);
  setTimeout(() => el.remove(), kind === 'error' ? 6000 : 3500);
}

function openModal({ title, body, footer, narrow, onClose }) {
  const close = () => { overlay.remove(); document.removeEventListener('keydown', onKey); if (onClose) onClose(); };
  const onKey = (e) => { if (e.key === 'Escape') close(); };
  const modal = h('div', { class: `modal ${narrow ? 'narrow' : ''}`, role: 'dialog', 'aria-modal': 'true' },
    h('div', { class: 'modal-head' }, h('h2', { text: title }),
      h('button', { class: 'icon', type: 'button', title: 'Close', on: { click: close } }, '✕')),
    h('div', { class: 'modal-body' }, body),
    footer ? h('div', { class: 'modal-foot' }, footer) : null);
  const overlay = h('div', { class: 'overlay', on: { mousedown: (e) => { if (e.target === overlay) close(); } } }, modal);
  document.addEventListener('keydown', onKey);
  $('#modal-root').append(overlay);
  const first = modal.querySelector('input, select, textarea');
  if (first) setTimeout(() => first.focus(), 0);
  return { close, modal };
}

/** Resolves true when confirmed. With `requireText`, the user must type it first. */
function confirmDialog({ title, message, confirmText, requireText }) {
  return new Promise((resolve) => {
    let done = false;
    const finish = (v) => { if (!done) { done = true; resolve(v); } };
    const input = requireText ? h('input', { type: 'text', placeholder: requireText, autocomplete: 'off' }) : null;
    const ok = h('button', { class: 'danger solid', type: 'button', disabled: !!requireText, text: confirmText || 'Delete' });
    if (input) input.addEventListener('input', () => { ok.disabled = input.value.trim() !== requireText; });
    const m = openModal({
      title, narrow: true, onClose: () => finish(false),
      body: [h('div', {}, message),
        input ? h('label', { class: 'field' }, ['Type ', h('b', { text: requireText }), ' to confirm'], input) : null],
      footer: [h('button', { type: 'button', text: 'Cancel', on: { click: () => m.close() } }), ok],
    });
    ok.addEventListener('click', () => { finish(true); m.close(); });
    if (input) input.addEventListener('keydown', (e) => { if (e.key === 'Enter' && !ok.disabled) ok.click(); });
  });
}

function showLogin(reason) {
  if ($('#login-form')) return;
  const input = h('input', { type: 'password', placeholder: 'Admin token', autocomplete: 'current-password' });
  const err = h('div', { class: 'form-error', text: reason || '' });
  const form = h('form', { id: 'login-form', class: 'modal-body' },
    h('div', { class: 'muted' }, 'This server requires an admin token (set with ', h('code', { text: '--admin-token' }), ' or ', h('code', { text: 'MEMHUB_ADMIN_TOKEN' }), ').'),
    h('label', { class: 'field' }, 'Token', input), err,
    h('button', { class: 'primary', type: 'submit', text: 'Sign in' }));
  const overlay = h('div', { class: 'overlay' },
    h('div', { class: 'modal login' }, h('div', { class: 'modal-head' }, h('h2', { text: 'Memory Hub admin' })), form));
  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    const res = await fetch('/api/admin/auth', { headers: { authorization: `Bearer ${input.value.trim()}` } });
    const info = await res.json();
    if (!info.authorized) { err.textContent = 'Wrong token.'; input.select(); return; }
    app.token = input.value.trim();
    localStorage.setItem(TOKEN_KEY, app.token);
    overlay.remove();
    $('#logout').classList.remove('hidden');
    start();
  });
  $('#modal-root').append(overlay);
  setTimeout(() => input.focus(), 0);
}

// ---------------------------------------------------------------------------
// Router & page chrome
// ---------------------------------------------------------------------------

const TABS = {
  dashboard: { title: 'Dashboard', view: viewDashboard },
  memories: { title: 'Memories', view: viewMemories },
  connections: { title: 'Connection log', view: viewConnections },
  updates: { title: 'Update log', view: viewUpdates },
};

function route() {
  const raw = location.hash.replace(/^#\/?/, '');
  const [tab, qs] = raw.split('?');
  return { tab: TABS[tab] ? tab : 'dashboard', params: new URLSearchParams(qs || '') };
}

/** Rewrites the current URL without triggering a re-render. */
function setRouteParams(tab, params) {
  history.replaceState(null, '', `#/${tab}${query(params)}`);
}

function setHead(title, sub, actions) {
  $('#page-title').textContent = title;
  $('#page-sub').textContent = sub || '';
  $('#page-actions').replaceChildren(...(actions || []));
  document.title = `${title} · Memory Hub`;
}

function every(ms, fn) {
  const t = setInterval(() => { if (!document.hidden) fn(); }, ms);
  app.timers.push(t);
}

function loading() {
  return h('div', { class: 'empty' }, h('span', { class: 'spinner' }), ' Loading…');
}

function errorBox(e) {
  return h('div', { class: 'empty' }, h('div', { class: 'form-error', text: e.message || String(e) }));
}

async function render() {
  app.timers.forEach(clearInterval);
  app.timers = [];
  const { tab, params } = route();
  document.querySelectorAll('.nav a').forEach((a) => a.classList.toggle('active', a.dataset.tab === tab));
  const view = $('#view');
  view.replaceChildren(loading());
  try {
    await TABS[tab].view(view, params);
  } catch (e) {
    if (e.status !== 401) view.replaceChildren(errorBox(e));
  }
}

function updateServerInfo(server) {
  if (!server) return;
  app.server = server;
  renderServerInfo();
  $('#logout').classList.toggle('hidden', !server.token_required);
}

function renderServerInfo() {
  const server = app.server;
  if (!server) return;
  $('#server-info').replaceChildren(
    h('div', {}, `memhub v${server.version}`),
    h('div', { title: fmtTime(server.started_at) }, `up ${fmtDuration((Date.now() - new Date(server.started_at)) / 1000)}`),
    h('div', { class: 'mono', title: 'Data directory' }, server.data_dir));
}

setInterval(renderServerInfo, 30000);

function refreshButton(fn) {
  return h('button', { type: 'button', title: 'Refresh', on: { click: fn } }, '↻ Refresh');
}

// ---------------------------------------------------------------------------
// Dashboard
// ---------------------------------------------------------------------------

async function viewDashboard(view) {
  const load = async () => {
    const st = await api('/stats');
    updateServerInfo(st.server);
    view.replaceChildren(renderDashboard(st));
  };
  setHead('Dashboard', 'Memories, agents and activity at a glance', [
    h('span', { class: 'muted', text: 'auto-refresh 10s' }), refreshButton(() => load().catch((e) => toast(e.message, 'error')))]);
  await load();
  every(10000, () => load().catch(() => {}));
}

function renderDashboard(st) {
  const live = st.live_sessions || [];
  const identified = live.filter((x) => x.agent).length;
  const totalChanges = st.activity.reduce((n, d) => n + d.changes, 0);
  const totalConnections = st.activity.reduce((n, d) => n + d.connections, 0);
  const card = (label, value, hint, href) => h('div', { class: 'panel card', on: { click: () => { location.hash = href; } } },
    h('div', { class: 'label', text: label }), h('div', { class: 'value', text: value }), h('div', { class: 'hint', text: hint }));

  const cards = h('div', { class: 'cards' },
    card('Memories', st.memories, `${st.repo_memories} repo · ${st.agent_memories} agent`, '#/memories'),
    card('Memory folders', st.repo_folders + st.agent_folders, `${st.repo_folders} repos · ${st.agent_folders} agents`, '#/memories'),
    card('Online now', live.length, `${identified} identified session${identified === 1 ? '' : 's'}`, '#/connections'),
    card('Connections today', st.connections_today, `${st.active_agents_today} active agent${st.active_agents_today === 1 ? '' : 's'}`, '#/connections?view=events&days=1'),
    card('Changes today', st.changes_today, `${totalChanges} in the last 14 days`, '#/updates?days=1'),
    card('Registered', st.registered_agents, `agents · ${st.registered_repos} repos`, '#/memories'));

  const entry = (icon, title, text, extra, href) => h('a', { class: 'panel entry', href },
    h('div', { class: 'big-ico', text: icon }),
    h('div', {}, h('h3', { text: title }), h('p', { text }), extra, h('span', { class: 'go', text: 'Open →' })));
  const kindChips = h('div', {}, st.kinds.slice(0, 6).map(([k, n]) => h('span', { class: 'chip', text: `${k} ${n}` })));
  const entries = h('div', { class: 'entries' },
    entry('❒', 'Memories', 'Browse every repo and agent memory with its properties; add or delete memories and folders.', kindChips, '#/memories'),
    entry('⇄', 'Connection log', `Who connected, from which IP, for how long. ${totalConnections} connections in 14 days.`, null, '#/connections'),
    entry('✎', 'Update log', `Every memory change with its diff. ${totalChanges} changes in 14 days.`, null, '#/updates'));

  const chartPanel = h('div', { class: 'panel' },
    h('div', { class: 'panel-head' }, h('h2', { text: 'Activity · last 14 days' }),
      h('div', { class: 'legend' }, h('span', {}, h('i', { class: 'c' }), 'connections'), h('span', {}, h('i', { class: 'u' }), 'memory changes'))),
    h('div', { class: 'panel-body chart' }, activityChart(st.activity)));

  const livePanel = h('div', { class: 'panel' },
    h('div', { class: 'panel-head' }, h('h2', { text: `Live sessions (${live.length})` }), h('a', { href: '#/connections', text: 'Connection log →' })),
    live.length ? h('ul', { class: 'feed' }, live.slice(0, 8).map((x) => h('li', {},
      h('span', { class: `dot ${x.busy ? '' : 'on'}` }),
      h('span', { class: 'what' }, h('b', {}, agentLabel(x.agent)), ' ', h('span', { class: 'muted', text: x.ip || '' })),
      h('span', { class: 'when', title: fmtTime(x.started_at) }, x.busy ? 'busy' : `idle ${fmtDuration(x.idle_secs)}`))))
      : h('div', { class: 'empty', text: 'No agent is connected right now.' }));

  const recent = st.recent_updates || [];
  const recentPanel = h('div', { class: 'panel' },
    h('div', { class: 'panel-head' }, h('h2', { text: 'Recent changes' }), h('a', { href: '#/updates', text: 'Update log →' })),
    recent.length ? h('ul', { class: 'feed' }, recent.map((r) => h('li', {},
      h('span', { class: `badge ${actionOf(r)}`, text: ACTION_LABELS[actionOf(r)] || actionOf(r) }),
      h('span', { class: 'what' }, h('b', {}, agentLabel(r.agent)), ' ',
        r.memory_id ? h('a', { href: memoryHash(r.memory_id), text: r.title || r.memory_id, title: r.memory_id })
          : h('span', { text: `${r.scope}/${r.owner}` })),
      h('span', { class: 'when', title: fmtTime(r.ts), text: ago(r.ts) }))))
      : h('div', { class: 'empty', text: 'No changes in the last 14 days.' }));

  const bars = (title, rows, link, empty) => {
    const max = Math.max(1, ...rows.map((r) => r[1]));
    return h('div', { class: 'panel' }, h('div', { class: 'panel-head' }, h('h2', { text: title })),
      h('div', { class: 'panel-body' }, rows.length ? h('div', { class: 'bars' }, rows.map(([name, n]) => {
        const fill = h('div', { class: 'fill' });
        fill.style.width = `${(n / max) * 100}%`;
        return h('div', { class: 'row' },
          h('a', { class: 'name', href: link(name), title: name }, name === '(admin)' ? agentLabel(name) : name),
          h('span', { class: 'num', text: n }), h('div', { class: 'track' }, fill));
      })) : h('div', { class: 'empty', text: empty })));
  };

  return h('div', {}, cards, entries,
    h('div', { class: 'grid-2' }, chartPanel, livePanel),
    h('div', { class: 'grid-3' }, recentPanel,
      bars('Most active agents · 14 days', st.top_agents, (a) => `#/updates${query({ agent: a })}`, 'No changes yet.'),
      bars('Largest memory folders', st.top_folders, (f) => {
        const [scope, owner] = f.split('/');
        return `#/memories${query({ scope, owner })}`;
      }, 'No memories yet.')));
}

function activityChart(days) {
  const W = 640, H = 190, top = 10, bottom = 22, left = 28;
  const max = Math.max(1, ...days.map((d) => Math.max(d.connections, d.changes)));
  const step = (W - left) / days.length;
  const bw = Math.max(4, step * 0.32);
  const y = (v) => top + (H - top - bottom) * (1 - v / max);
  const svg = s('svg', { viewBox: `0 0 ${W} ${H}`, preserveAspectRatio: 'none', role: 'img' });
  for (const frac of [0, 0.5, 1]) {
    const v = Math.round(max * frac);
    svg.append(s('line', { class: 'grid', x1: left, x2: W, y1: y(v), y2: y(v) }),
      s('text', { class: 'axis', x: 0, y: y(v) + 3 }, v));
  }
  days.forEach((d, i) => {
    const x = left + i * step + (step - 2 * bw - 2) / 2;
    const label = `${d.date}: ${d.connections} connections, ${d.changes} changes, ${d.agents} agents`;
    svg.append(
      s('rect', { class: 'bar-c', x, y: y(d.connections), width: bw, height: Math.max(0, H - bottom - y(d.connections)), rx: 2 }, s('title', {}, label)),
      s('rect', { class: 'bar-u', x: x + bw + 2, y: y(d.changes), width: bw, height: Math.max(0, H - bottom - y(d.changes)), rx: 2 }, s('title', {}, label)));
    if (i % 2 === days.length % 2 || i === days.length - 1) {
      svg.append(s('text', { class: 'axis', x: left + i * step + step / 2, y: H - 6, 'text-anchor': 'middle' }, d.date.slice(5)));
    }
  });
  return svg;
}

// ---------------------------------------------------------------------------
// Memories
// ---------------------------------------------------------------------------

async function viewMemories(view, params) {
  const st = {
    scope: params.get('scope') || '',
    owner: params.get('owner') || '',
    q: params.get('q') || '',
    id: params.get('id') || '',
    folders: [],
    mode: 'preview',
  };
  const sync = () => setRouteParams('memories', { scope: st.scope, owner: st.owner, q: st.q, id: st.id });

  const treeBox = h('div', { class: 'scroll' });
  const listBox = h('div', { class: 'scroll' });
  const listCount = h('span', { class: 'muted' });
  const detailBox = h('div', { class: 'scroll' });
  const search = h('input', { type: 'search', placeholder: 'Search title, tags, body…', value: st.q });

  setHead('Memories', 'Every repo and agent memory, stored as Markdown files with front-matter properties', [
    h('button', { type: 'button', on: { click: () => newFolderModal() } }, '+ New folder'),
    h('button', { class: 'primary', type: 'button', on: { click: () => newMemoryModal() } }, '+ New memory')]);

  view.replaceChildren(h('div', { class: 'mem-layout' },
    h('div', { class: 'panel' }, h('div', { class: 'panel-head' }, h('h2', { text: 'Folders' })), treeBox),
    h('div', { class: 'panel' }, h('div', { class: 'search-row' }, search), h('div', { class: 'panel-head' }, h('h2', { id: 'list-title' }), listCount), listBox),
    h('div', { class: 'panel' }, detailBox)));

  search.addEventListener('input', debounce(() => { st.q = search.value.trim(); sync(); loadList(); }, 250));

  async function loadTree() {
    const data = await api('/folders');
    st.folders = data.folders;
    renderTree();
  }

  function renderTree() {
    const total = st.folders.reduce((n, f) => n + f.memory_count, 0);
    const item = (label, count, active, onClick, extra) => h('div', { class: `folder ${active ? 'active' : ''}`, on: { click: onClick } },
      h('span', { class: 'name', title: label, text: label }), extra, h('span', { class: 'count', text: count }));
    const section = (scope, title) => {
      const folders = st.folders.filter((f) => f.scope === scope);
      return h('div', { class: 'tree-section' },
        h('div', { class: 'tree-title' }, h('span', { text: `${title} (${folders.length})` }),
          h('button', { class: 'icon', type: 'button', title: `New ${scope} folder`, on: { click: () => newFolderModal(scope) } }, '+')),
        item(`All ${scope} memories`, folders.reduce((n, f) => n + f.memory_count, 0), st.scope === scope && !st.owner, () => select(scope, '')),
        folders.map((f) => item(f.owner, f.memory_count, st.scope === scope && st.owner === f.owner, () => select(scope, f.owner), [
          f.registered ? null : h('span', { class: 'unreg', title: scope === 'repo' ? 'No agent has registered this repo with add_repo' : 'No agent with this name has connected', text: 'unregistered' }),
          h('button', { class: 'icon del', type: 'button', title: `Delete folder ${scope}/${f.owner}`, on: { click: (e) => { e.stopPropagation(); deleteFolder(f); } } }, '🗑'),
        ])));
    };
    treeBox.replaceChildren(
      h('div', { class: 'tree-section' }, item('All memories', total, !st.scope, () => select('', ''))),
      section('repo', 'Repos'), section('agent', 'Agents'));
  }

  function select(scope, owner) {
    st.scope = scope; st.owner = owner;
    sync(); renderTree(); loadList();
  }

  async function loadList() {
    const title = st.owner ? `${st.scope}/${st.owner}` : st.scope ? `All ${st.scope} memories` : 'All memories';
    $('#list-title').textContent = st.q ? `Search in ${title}` : title;
    listBox.replaceChildren(loading());
    try {
      const data = await api('/memories' + query({ scope: st.scope, owner: st.owner, q: st.q }));
      listCount.textContent = `${data.count}`;
      if (!data.memories.length) {
        listBox.replaceChildren(h('div', { class: 'empty' }, st.q ? 'No memory matches the search.' : 'This folder is empty.',
          h('div', {}, h('button', { class: 'link', type: 'button', text: '+ Add a memory', on: { click: () => newMemoryModal() } }))));
        return;
      }
      listBox.replaceChildren(...data.memories.map((m) => h('div', {
        class: `mem-item ${m.id === st.id ? 'active' : ''}`, dataset: { id: m.id },
        on: { click: () => { st.id = m.id; sync(); markActive(); loadDetail(); } },
      },
      h('div', { class: 't', text: m.title, title: m.title }),
      h('div', { class: 'id mono', text: m.id, title: m.id }),
      h('div', { class: 'meta' },
        st.owner ? null : h('span', { class: `badge ${m.id.split('/')[0]}`, text: m.id.split('/')[0] }),
        h('span', { class: 'chip', text: m.kind }),
        (m.tags || []).slice(0, 3).map((t) => h('span', { class: 'chip', text: `#${t}` })),
        h('span', { title: fmtTime(m.updated_at), text: ago(m.updated_at) })),
      m.snippet ? h('div', { class: 'snip', text: m.snippet }) : null)));
    } catch (e) {
      listBox.replaceChildren(errorBox(e));
    }
  }

  function markActive() {
    listBox.querySelectorAll('.mem-item').forEach((el) => el.classList.toggle('active', el.dataset.id === st.id));
  }

  async function loadDetail() {
    if (!st.id) {
      detailBox.replaceChildren(h('div', { class: 'empty' }, 'Select a memory to see its properties and content.'));
      return;
    }
    detailBox.replaceChildren(loading());
    try {
      const d = await api('/memories/' + st.id.split('/').map(encodeURIComponent).join('/'));
      detailBox.replaceChildren(renderDetail(d));
    } catch (e) {
      detailBox.replaceChildren(h('div', { class: 'empty' }, h('div', { class: 'form-error', text: e.message }),
        e.status === 404 ? h('div', { class: 'muted', text: 'It may have been deleted. Check the update log for its history.' }) : null));
    }
  }

  function renderDetail(d) {
    const m = d.memory;
    const [scope, owner] = m.id.split('/');
    const chips = (list) => (list && list.length ? list.map((t) => h('span', { class: 'chip', text: t })) : h('span', { class: 'muted', text: '—' }));
    const known = new Set(['id', 'title', 'kind', 'tags', 'groups', 'author', 'updated_by', 'created_at', 'updated_at', 'version', 'body']);
    const extra = Object.entries(m).filter(([k]) => !known.has(k));
    const row = (k, v) => h('tr', {}, h('th', { text: k }), h('td', {}, v));
    const bodyBox = h('div');
    const showBody = () => {
      if (st.mode === 'raw') bodyBox.replaceChildren(h('pre', { class: 'raw', text: d.markdown }));
      else if (!m.body.trim()) bodyBox.replaceChildren(h('div', { class: 'muted', text: 'Empty body.' }));
      else { const md = h('div', { class: 'md' }); md.innerHTML = renderMarkdown(m.body); bodyBox.replaceChildren(md); }
      seg.querySelectorAll('button').forEach((b) => b.classList.toggle('on', b.dataset.mode === st.mode));
    };
    const seg = h('div', { class: 'seg' },
      h('button', { type: 'button', dataset: { mode: 'preview' }, text: 'Preview', on: { click: () => { st.mode = 'preview'; showBody(); } } }),
      h('button', { type: 'button', dataset: { mode: 'raw' }, text: 'File', on: { click: () => { st.mode = 'raw'; showBody(); } } }));
    showBody();

    return h('div', {},
      h('div', { class: 'detail-head' }, h('h2', { text: m.title }), h('div', { class: 'sub mono', text: d.path }),
        h('div', { class: 'actions' },
          h('button', { class: 'small', type: 'button', text: 'Copy id', on: { click: () => copy(m.id) } }),
          h('a', { class: 'btn small', href: `#/updates${query({ memory: m.id })}`, text: 'History' }),
          h('button', { class: 'small danger', type: 'button', text: 'Delete', on: { click: () => deleteMemory(m) } }))),
      h('div', { class: 'body-box' },
        h('table', { class: 'props' }, h('tbody', {},
          row('id', h('span', { class: 'mono', text: m.id })),
          row('scope', h('span', { class: `badge ${scope}`, text: scope })),
          row(scope === 'repo' ? 'repo' : 'agent', owner),
          row('kind', h('span', { class: 'chip', text: m.kind })),
          row('tags', chips(m.tags)),
          row('groups', chips(m.groups)),
          row('author', agentLabel(m.author)),
          row('updated by', m.updated_by ? agentLabel(m.updated_by) : h('span', { class: 'muted', text: '—' })),
          row('created', h('span', { title: m.created_at, text: `${fmtTime(m.created_at)} (${ago(m.created_at)})` })),
          row('updated', h('span', { title: m.updated_at, text: `${fmtTime(m.updated_at)} (${ago(m.updated_at)})` })),
          row('version', String(m.version)),
          extra.map(([k, v]) => row(k, h('span', { class: 'mono', text: typeof v === 'string' ? v : JSON.stringify(v) }))))),
        h('div', { class: 'body-tabs' }, h('h2', { text: 'Content' }), seg), bodyBox));
  }

  async function deleteMemory(m) {
    const ok = await confirmDialog({
      title: 'Delete memory',
      message: h('div', {}, 'Move ', h('b', { text: m.title }), ' (', h('code', { text: m.id }), ') to the trash? Agents will no longer see it. The deletion is recorded in the update log.'),
    });
    if (!ok) return;
    try {
      await api('/memories/' + m.id.split('/').map(encodeURIComponent).join('/'), { method: 'DELETE' });
      toast(`Deleted ${m.id}`);
      st.id = ''; sync();
      await Promise.all([loadTree(), loadList(), loadDetail()]);
    } catch (e) { toast(e.message, 'error'); }
  }

  async function deleteFolder(f) {
    const path = `${f.scope}/${f.owner}`;
    const ok = await confirmDialog({
      title: 'Delete folder',
      message: h('div', {}, 'Move the folder ', h('code', { text: path }), f.memory_count ? [' and its ', h('b', { text: `${f.memory_count} memories` })] : ' (empty)',
        ' to the trash? The ', f.scope === 'repo' ? 'repo' : 'agent', ' registration is kept, so agents can write here again.'),
      requireText: f.memory_count ? f.owner : null,
      confirmText: 'Delete folder',
    });
    if (!ok) return;
    try {
      const res = await api(`/folders/${encodeURIComponent(f.scope)}/${encodeURIComponent(f.owner)}`, { method: 'DELETE' });
      toast(`Deleted folder ${path} (${res.deleted.length} memories)`);
      if (st.owner === f.owner && st.scope === f.scope) { st.owner = ''; }
      if (st.id.startsWith(path + '/')) st.id = '';
      sync();
      await Promise.all([loadTree(), loadList(), loadDetail()]);
    } catch (e) { toast(e.message, 'error'); }
  }

  function newFolderModal(scope) {
    let current = scope || st.scope || 'repo';
    const name = h('input', { type: 'text', placeholder: current === 'repo' ? 'payment-service' : 'crash-analyzer', autocomplete: 'off' });
    const err = h('div', { class: 'form-error' });
    const seg = h('div', { class: 'seg' }, ['repo', 'agent'].map((sc) => h('button', {
      type: 'button', dataset: { scope: sc }, class: sc === current ? 'on' : '', text: sc === 'repo' ? 'Repo' : 'Agent',
      on: { click: () => { current = sc; seg.querySelectorAll('button').forEach((b) => b.classList.toggle('on', b.dataset.scope === sc)); name.placeholder = sc === 'repo' ? 'payment-service' : 'crash-analyzer'; } },
    })));
    const submit = async () => {
      err.textContent = '';
      try {
        const res = await api('/folders', { method: 'POST', body: { scope: current, owner: name.value } });
        toast(res.created ? `Created folder ${current}/${res.owner}` : `Folder ${current}/${res.owner} already exists`);
        m.close();
        await loadTree();
        select(current, res.owner);
      } catch (e) { err.textContent = e.message; }
    };
    name.addEventListener('keydown', (e) => { if (e.key === 'Enter') submit(); });
    const m = openModal({
      title: 'New memory folder', narrow: true,
      body: [h('label', { class: 'field' }, 'Type', seg), h('label', { class: 'field' }, 'Name (repo id or agent name)', name),
        h('div', { class: 'note', text: 'Letters, digits, "-", "_" and "." only. A folder holds the memories of one repo or one agent.' }), err],
      footer: [h('button', { type: 'button', text: 'Cancel', on: { click: () => m.close() } }), h('button', { class: 'primary', type: 'button', text: 'Create', on: { click: submit } })],
    });
  }

  function newMemoryModal() {
    const scope = h('select', {}, h('option', { value: 'repo', text: 'repo — shared repository knowledge' }), h('option', { value: 'agent', text: 'agent — an agent\'s own notebook' }));
    scope.value = st.scope || 'repo';
    const ownerList = h('datalist', { id: 'owner-options' });
    const owner = h('input', { type: 'text', list: 'owner-options', value: st.owner, autocomplete: 'off' });
    const fillOwners = () => ownerList.replaceChildren(...st.folders.filter((f) => f.scope === scope.value).map((f) => h('option', { value: f.owner })));
    scope.addEventListener('change', () => { fillOwners(); if (!st.folders.some((f) => f.scope === scope.value && f.owner === owner.value)) owner.value = ''; });
    fillOwners();
    const title = h('input', { type: 'text', autocomplete: 'off' });
    const kindList = h('datalist', { id: 'kind-options' }, ['overview', 'architecture', 'implementation', 'pitfall', 'task-log', 'insight', 'todo', 'note'].map((k) => h('option', { value: k })));
    const kind = h('input', { type: 'text', list: 'kind-options', placeholder: 'note' });
    const tags = h('input', { type: 'text', placeholder: 'comma, separated' });
    const groups = h('input', { type: 'text', placeholder: 'comma, separated' });
    const slug = h('input', { type: 'text', placeholder: 'derived from the title', autocomplete: 'off' });
    const body = h('textarea', { rows: 12, placeholder: 'Markdown content…' });
    const useFm = h('input', { type: 'checkbox' });
    const fmNote = h('div', { class: 'note hidden' });
    const file = h('input', { type: 'file', accept: '.md,.markdown,.txt,text/markdown,text/plain', class: 'hidden' });
    const err = h('div', { class: 'form-error' });
    const updateFm = () => {
      fmNote.classList.toggle('hidden', !useFm.checked);
      fmNote.textContent = 'The content is a complete memory file: title, kind, tags and groups are read from its front matter (or first "# heading") unless you fill them in above.';
      title.placeholder = useFm.checked ? 'from front matter / first heading' : '';
    };
    useFm.addEventListener('change', updateFm);
    file.addEventListener('change', async () => {
      const f = file.files[0];
      if (!f) return;
      body.value = await f.text();
      useFm.checked = /^\uFEFF?---\r?\n/.test(body.value) || /^#\s/m.test(body.value);
      if (!slug.value) slug.value = f.name.replace(/\.(md|markdown|txt)$/i, '').toLowerCase().replace(/[^a-z0-9._-]+/g, '-').replace(/^[^a-z0-9]+|-+$/g, '');
      updateFm();
      toast(`Loaded ${f.name}`);
    });
    const list = (v) => v.split(',').map((x) => x.trim()).filter(Boolean);
    const submit = async () => {
      err.textContent = '';
      if (!owner.value.trim()) { err.textContent = 'Choose the repo or agent folder.'; return; }
      if (!useFm.checked && !title.value.trim()) { err.textContent = 'A title is required.'; return; }
      const payload = {
        scope: scope.value, owner: owner.value.trim(), title: title.value, kind: kind.value,
        tags: list(tags.value), groups: list(groups.value), slug: slug.value,
      };
      if (useFm.checked) payload.markdown = body.value; else payload.body = body.value;
      try {
        const res = await api('/memories', { method: 'POST', body: payload });
        toast(`Created ${res.created.id}`);
        m.close();
        st.scope = scope.value; st.owner = payload.owner; st.id = res.created.id; st.q = ''; search.value = '';
        sync();
        await Promise.all([loadTree(), loadList(), loadDetail()]);
      } catch (e) { err.textContent = e.message; }
    };
    const m = openModal({
      title: 'New memory',
      body: [
        h('div', { class: 'row2' }, h('label', { class: 'field' }, 'Scope', scope), h('label', { class: 'field' }, 'Folder (repo id / agent name)', owner, ownerList)),
        h('label', { class: 'field' }, 'Title', title),
        h('div', { class: 'row2' }, h('label', { class: 'field' }, 'Kind', kind, kindList), h('label', { class: 'field' }, 'File name (slug)', slug)),
        h('div', { class: 'row2' }, h('label', { class: 'field' }, 'Tags', tags), h('label', { class: 'field' }, 'Groups', groups)),
        h('div', { class: 'body-tabs' },
          h('label', { class: 'nowrap' }, useFm, ' Content includes front matter'),
          h('button', { class: 'small', type: 'button', text: '⇪ Import .md file', on: { click: () => file.click() } }), file),
        fmNote, body,
        h('div', { class: 'note', text: 'Created as author "(admin)". The change is recorded in the update log.' }), err],
      footer: [h('button', { type: 'button', text: 'Cancel', on: { click: () => m.close() } }), h('button', { class: 'primary', type: 'button', text: 'Create memory', on: { click: submit } })],
    });
    if (st.owner) setTimeout(() => title.focus(), 0);
  }

  await loadTree();
  await Promise.all([loadList(), loadDetail()]);
}

function copy(text) {
  const done = () => toast(`Copied ${text}`);
  if (navigator.clipboard && window.isSecureContext) {
    navigator.clipboard.writeText(text).then(done, () => fallbackCopy(text, done));
  } else {
    fallbackCopy(text, done);
  }
}

function fallbackCopy(text, done) {
  const ta = h('textarea', { class: 'hidden-copy' });
  ta.value = text;
  document.body.append(ta);
  ta.select();
  try { document.execCommand('copy'); done(); } catch (_) { toast('Copy failed', 'error'); }
  ta.remove();
}

// ---------------------------------------------------------------------------
// Log views
// ---------------------------------------------------------------------------

const RANGES = [['1', 'Last 24 hours'], ['7', 'Last 7 days'], ['30', 'Last 30 days'], ['90', 'Last 90 days'], ['', 'All time']];
const PAGE = 200;

function filterInput(placeholder, value, onChange, listId) {
  const input = h('input', { type: 'search', placeholder, value: value || '', list: listId || null });
  input.addEventListener('input', debounce(() => onChange(input.value.trim()), 300));
  return input;
}

function rangeSelect(value, onChange) {
  const sel = h('select', {}, RANGES.map(([v, label]) => h('option', { value: v, text: label })));
  sel.value = value;
  sel.addEventListener('change', () => onChange(sel.value));
  return sel;
}

async function viewConnections(view, params) {
  const st = {
    view: params.get('view') === 'events' ? 'events' : 'sessions',
    agent: params.get('agent') || '',
    ip: params.get('ip') || '',
    days: params.has('days') ? params.get('days') : '7',
    limit: PAGE,
  };
  const sync = () => setRouteParams('connections', { view: st.view === 'events' ? 'events' : '', agent: st.agent, ip: st.ip, days: st.days === '7' ? '' : (st.days || 'all') });
  if (st.days === 'all') st.days = '';
  const agents = h('datalist', { id: 'agent-options' });
  const body = h('div', { class: 'table-wrap' });
  const foot = h('div');
  const seg = h('div', { class: 'seg' }, [['sessions', 'Sessions'], ['events', 'Raw events']].map(([v, label]) => h('button', {
    type: 'button', dataset: { v }, class: st.view === v ? 'on' : '', text: label,
    on: { click: () => { st.view = v; st.limit = PAGE; seg.querySelectorAll('button').forEach((b) => b.classList.toggle('on', b.dataset.v === v)); sync(); load(); } },
  })));
  const change = (k) => (v) => { st[k] = v; st.limit = PAGE; sync(); load(); };

  setHead('Connection log', 'MCP connections and disconnections with agent name and client IP', [refreshButton(() => load())]);
  view.replaceChildren(h('div', { class: 'panel' },
    h('div', { class: 'filters' }, seg,
      filterInput('Agent', st.agent, change('agent'), 'agent-options'), agents,
      filterInput('IP address', st.ip, change('ip')),
      rangeSelect(st.days, change('days')), h('span', { class: 'grow' }),
      h('span', { class: 'muted', id: 'conn-count' })),
    body, foot));

  async function load() {
    body.replaceChildren(loading());
    foot.replaceChildren();
    try {
      const data = await api('/connections' + query({ view: st.view, agent: st.agent, ip: st.ip, days: st.days, limit: st.limit }));
      const rows = st.view === 'events' ? data.events : data.sessions;
      const names = new Set(rows.map((r) => r.agent).filter((a) => a && a !== '(admin)'));
      agents.replaceChildren(...[...names].sort().map((a) => h('option', { value: a })));
      $('#conn-count').textContent = `${rows.length}${rows.length === st.limit ? '+' : ''} ${st.view === 'events' ? 'events' : 'sessions'}`;
      if (!rows.length) { body.replaceChildren(h('div', { class: 'empty', text: 'No connections match these filters.' })); return; }
      body.replaceChildren(st.view === 'events' ? eventsTable(rows) : sessionsTable(rows));
      if (rows.length === st.limit) {
        foot.replaceChildren(h('div', { class: 'empty' }, h('button', { type: 'button', text: 'Show more', on: { click: () => { st.limit += PAGE; load(); } } })));
      }
    } catch (e) { body.replaceChildren(errorBox(e)); }
  }

  const filterAgent = (name) => h('a', { href: '#', on: { click: (e) => { e.preventDefault(); st.agent = name; sync(); render(); } } }, name);

  function sessionsTable(rows) {
    return h('table', {},
      h('thead', {}, h('tr', {}, ['Status', 'Agent', 'IP', 'Client', 'Connected', 'Duration', 'Ended', 'Changes', 'Session'].map((c) => h('th', { text: c })))),
      h('tbody', {}, rows.map((r) => {
        const status = r.live ? h('span', { class: 'badge live' }, h('span', { class: 'dot on' }), 'live')
          : r.disconnected_at ? h('span', { class: 'badge', text: 'closed' })
            : h('span', { class: 'badge', title: 'No disconnect recorded (server restarted or log window cut)', text: 'unknown' });
        const duration = r.live && r.connected_at ? (Date.now() - new Date(r.connected_at)) / 1000 : r.duration_secs;
        return h('tr', {},
          h('td', {}, status),
          h('td', {}, r.agent ? filterAgent(r.agent) : agentLabel(null), r.registered ? [' ', h('span', { class: 'badge registered', title: 'Agent registered by this connection', text: 'new' })] : null),
          h('td', { class: 'mono nowrap', text: r.ip }),
          h('td', { class: 'wrap', text: r.client || '—' }),
          timeCell(r.connected_at),
          h('td', { class: 'nowrap', text: fmtDuration(duration) }),
          h('td', { class: 'nowrap' }, r.disconnected_at ? [h('span', { title: fmtTime(r.disconnected_at), text: r.reason || '' })] : h('span', { class: 'muted', text: '—' })),
          h('td', {}, r.changes && r.agent ? h('a', { href: `#/updates${query({ agent: r.agent })}`, text: r.changes }) : String(r.changes || 0)),
          h('td', { class: 'mono', title: r.session_id, text: shortId(r.session_id) }));
      })));
  }

  function eventsTable(rows) {
    const detail = (r) => {
      switch (r.event) {
        case 'connect': return r.client ? `client ${r.client}` : '';
        case 'disconnect': return `${r.reason} after ${fmtDuration(r.duration_secs)}`;
        case 'agent_registered': return `registered with role ${r.role}`;
        case 'agent_login': return 'identified';
        default: return '';
      }
    };
    return h('table', {},
      h('thead', {}, h('tr', {}, ['Time', 'Event', 'Agent', 'IP', 'Transport', 'Detail', 'Session'].map((c) => h('th', { text: c })))),
      h('tbody', {}, rows.map((r) => h('tr', {},
        timeCell(r.ts),
        h('td', {}, h('span', { class: `badge ${r.event === 'connect' ? 'add' : r.event === 'disconnect' ? 'delete' : 'registered'}`, text: r.event })),
        h('td', {}, r.agent ? filterAgent(r.agent) : agentLabel(null)),
        h('td', { class: 'mono nowrap', text: r.ip }),
        h('td', { text: r.transport }),
        h('td', { class: 'wrap', text: detail(r) }),
        h('td', { class: 'mono', title: r.session_id, text: shortId(r.session_id) })))));
  }

  await load();
}

async function viewUpdates(view, params) {
  const st = {
    agent: params.get('agent') || '',
    action: params.get('action') || '',
    memory: params.get('memory') || '',
    days: params.has('days') ? params.get('days') : '30',
    limit: PAGE,
  };
  if (st.days === 'all') st.days = '';
  const sync = () => setRouteParams('updates', { agent: st.agent, action: st.action, memory: st.memory, days: st.days === '30' ? '' : (st.days || 'all') });
  const change = (k) => (v) => { st[k] = v; st.limit = PAGE; sync(); load(); };
  const agents = h('datalist', { id: 'upd-agent-options' });
  const action = h('select', {}, h('option', { value: '', text: 'All actions' }),
    Object.entries(ACTION_LABELS).map(([v, label]) => h('option', { value: v, text: label })));
  action.value = st.action;
  action.addEventListener('change', () => change('action')(action.value));
  const body = h('div', { class: 'table-wrap' });
  const foot = h('div');
  const count = h('span', { class: 'muted' });

  setHead('Update log', 'How agents (and admins) changed memory: adds, edits, deletions and grouping, with diffs', [refreshButton(() => load())]);
  view.replaceChildren(h('div', { class: 'panel' },
    h('div', { class: 'filters' },
      filterInput('Agent', st.agent, change('agent'), 'upd-agent-options'), agents, action,
      filterInput('Memory id or title', st.memory, change('memory')),
      rangeSelect(st.days, change('days')), h('span', { class: 'grow' }), count),
    body, foot));

  async function load() {
    body.replaceChildren(loading());
    foot.replaceChildren();
    try {
      const data = await api('/updates' + query({ agent: st.agent, action: st.action, memory: st.memory, days: st.days, limit: st.limit }));
      const rows = data.updates;
      agents.replaceChildren(...[...new Set(rows.map((r) => r.agent).filter(Boolean))].sort().map((a) => h('option', { value: a })));
      count.textContent = `${rows.length}${rows.length === st.limit ? '+' : ''} changes`;
      if (!rows.length) { body.replaceChildren(h('div', { class: 'empty', text: 'No changes match these filters.' })); return; }
      body.replaceChildren(updatesTable(rows));
      if (rows.length === st.limit) {
        foot.replaceChildren(h('div', { class: 'empty' }, h('button', { type: 'button', text: 'Show more', on: { click: () => { st.limit += PAGE; load(); } } })));
      }
    } catch (e) { body.replaceChildren(errorBox(e)); }
  }

  function versions(r) {
    if (r.version_before && r.version_after) return `v${r.version_before} → v${r.version_after}`;
    if (r.version_after) return `v${r.version_after}`;
    if (r.version_before) return `was v${r.version_before}`;
    return '';
  }

  function diffView(diff) {
    const pre = h('pre', { class: 'diff' });
    for (const line of diff.replace(/\n$/, '').split('\n')) {
      const cls = line.startsWith('@@') ? 'hunk' : line.startsWith('+++') || line.startsWith('---') ? 'hdr'
        : line.startsWith('+') ? 'add' : line.startsWith('-') ? 'del' : '';
      pre.append(h('span', { class: cls, text: line || ' ' }));
    }
    return pre;
  }

  function updatesTable(rows) {
    const cols = ['Time', 'Agent', 'Action', 'Memory', 'Version', 'Changed', 'IP', ''];
    const tbody = h('tbody');
    for (const r of rows) {
      const act = actionOf(r);
      const target = r.memory_id
        ? h('div', {}, h('div', { text: r.title || '' }), h('a', { class: 'mono', href: memoryHash(r.memory_id), text: r.memory_id }))
        : h('div', {}, h('span', { class: 'mono', text: `${r.scope}/${r.owner}` }),
          r.event === 'folder_deleted' ? h('div', { class: 'muted', text: `${r.memory_count} memories moved to trash` }) : null);
      const changed = [
        (r.changed_fields || []).map((f) => h('span', { class: 'chip', text: f })),
        r.group ? h('span', { class: 'chip', text: `group: ${r.group}` }) : null,
      ];
      const toggle = r.diff ? h('button', { class: 'small', type: 'button', text: 'Diff' }) : null;
      const tr = h('tr', { class: r.diff ? 'expandable' : '' },
        timeCell(r.ts),
        h('td', {}, agentLabel(r.agent)),
        h('td', {}, h('span', { class: `badge ${act}`, text: ACTION_LABELS[act] || act })),
        h('td', { class: 'wrap' }, target),
        h('td', { class: 'nowrap', text: versions(r) }),
        h('td', {}, changed),
        h('td', { class: 'mono nowrap', text: r.ip }),
        h('td', {}, toggle));
      tbody.append(tr);
      if (r.diff) {
        let detail = null;
        tr.addEventListener('click', (e) => {
          if (e.target.closest('a')) return;
          if (detail) { detail.remove(); detail = null; toggle.textContent = 'Diff'; return; }
          detail = h('tr', { class: 'detail-row' }, h('td', { colspan: cols.length }, diffView(r.diff)));
          tr.after(detail);
          toggle.textContent = 'Hide';
        });
      }
    }
    return h('table', {}, h('thead', {}, h('tr', {}, cols.map((c) => h('th', { text: c })))), tbody);
  }

  await load();
}

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

$('#logout').addEventListener('click', () => {
  localStorage.removeItem(TOKEN_KEY);
  app.token = '';
  location.reload();
});

window.addEventListener('hashchange', render);

(async function boot() {
  try {
    const res = await fetch('/api/admin/auth', { headers: app.token ? { authorization: `Bearer ${app.token}` } : {} });
    const info = await res.json();
    if (info.token_required) $('#logout').classList.remove('hidden');
    if (info.token_required && !info.authorized) { showLogin(app.token ? 'The saved token is no longer valid.' : ''); return; }
  } catch (e) {
    $('#view').replaceChildren(errorBox(new Error(`Cannot reach the server: ${e.message}`)));
    return;
  }
  start();
})();

/** Runs once access is granted (at boot or after signing in). */
function start() {
  if (route().tab !== 'dashboard') api('/stats').then((st) => updateServerInfo(st.server)).catch(() => {});
  render();
}
