// Drover administration interface. Plain JavaScript, no dependencies.
// Every value coming from the server is rendered with textContent (via `h`),
// never as HTML.
'use strict';

const TOKEN_KEY = 'drover.adminToken';
const state = {
  token: safeGet(TOKEN_KEY) || '',
  languages: [],
  settings: [],
};

function safeGet(key) {
  try { return sessionStorage.getItem(key); } catch { return null; }
}
function safeSet(key, value) {
  try { value ? sessionStorage.setItem(key, value) : sessionStorage.removeItem(key); } catch { /* ignore */ }
}

// --- DOM helpers ----------------------------------------------------------

function h(tag, attrs, ...children) {
  const el = document.createElement(tag);
  for (const [key, value] of Object.entries(attrs || {})) {
    if (value === undefined || value === null || value === false) continue;
    if (key.startsWith('on')) el.addEventListener(key.slice(2), value);
    else if (key === 'class') el.className = value;
    else if (key === 'value') el.value = value;
    else if (key === 'checked') el.checked = !!value;
    else el.setAttribute(key, value === true ? '' : value);
  }
  for (const child of children.flat(Infinity)) {
    if (child === undefined || child === null || child === false) continue;
    el.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
  return el;
}

const view = () => document.getElementById('view');
function render(...nodes) {
  view().replaceChildren(...nodes.flat(Infinity).filter(n => n !== null && n !== undefined && n !== false));
}

function badge(text, cls) {
  return h('span', { class: `badge ${cls || text}` }, text);
}

function langBadge(id) {
  const lang = state.languages.find(l => l.id === id);
  return badge(lang ? lang.name : id, id);
}

function toast(message, kind) {
  const el = h('div', { class: `toast ${kind || ''}` }, message);
  document.getElementById('toasts').append(el);
  setTimeout(() => el.remove(), kind === 'error' ? 8000 : 3500);
}

function errorBanner(err) {
  return h('div', { class: 'banner error' },
    h('div', {}, err.message || String(err)),
    err.code ? h('div', { class: 'code' }, err.code) : null);
}

function field(labelText, control, hint) {
  return h('div', { class: 'field' }, h('label', {}, labelText), control, hint ? h('div', { class: 'hint' }, hint) : null);
}

function fmtJson(value) {
  return JSON.stringify(value, null, 2);
}

function parseJson(text, what) {
  try {
    return JSON.parse(text);
  } catch (e) {
    throw new Error(`${what} is not valid JSON: ${e.message}`);
  }
}

function debounce(fn, ms) {
  let timer;
  return (...args) => { clearTimeout(timer); timer = setTimeout(() => fn(...args), ms); };
}

// --- API --------------------------------------------------------------------

class ApiError extends Error {
  constructor(status, code, message, details) {
    super(message);
    this.status = status;
    this.code = code;
    this.details = details;
  }
}

async function api(method, path, body) {
  const headers = { Authorization: `Bearer ${state.token}` };
  if (body !== undefined) headers['Content-Type'] = 'application/json';
  let res;
  try {
    res = await fetch(`/admin/api${path}`, { method, headers, body: body !== undefined ? JSON.stringify(body) : undefined });
  } catch (e) {
    throw new ApiError(0, 'network_error', `Cannot reach Drover: ${e.message}`);
  }
  const text = await res.text();
  let data = null;
  try { data = text ? JSON.parse(text) : null; } catch { /* not JSON */ }
  if (res.status === 401) {
    signOut('The admin token was rejected.');
    throw new ApiError(401, 'unauthorized', 'The admin token was rejected.');
  }
  if (!res.ok) {
    const err = data && data.error ? data.error : {};
    throw new ApiError(res.status, err.code || `http_${res.status}`, err.message || text || res.statusText, err.details);
  }
  return data;
}

function notifyApplied(result, what) {
  if (result && result.restart_required) {
    toast(`${what} saved. It takes effect after a restart.`);
  } else {
    toast(`${what} saved and applied (rules version ${result.rules_version}).`);
  }
  for (const w of (result && result.warnings) || []) toast(w, 'error');
}

// --- shell ------------------------------------------------------------------

function signOut(message) {
  state.token = '';
  safeSet(TOKEN_KEY, '');
  document.getElementById('nav').hidden = true;
  document.getElementById('logout').hidden = true;
  showLogin(message);
}

function showLogin(message) {
  const input = h('input', { type: 'password', autocomplete: 'current-password', placeholder: 'auth.admin_token' });
  const error = h('div');
  const submit = async e => {
    e.preventDefault();
    state.token = input.value.trim();
    error.replaceChildren();
    try {
      await bootstrap();
      safeSet(TOKEN_KEY, state.token);
      route();
    } catch (err) {
      state.token = '';
      error.replaceChildren(errorBanner(err));
    }
  };
  render(h('form', { class: 'card login', onsubmit: submit },
    h('h1', {}, 'Drover administration'),
    h('p', { class: 'muted' }, 'Sign in with the admin token configured in auth.admin_token.'),
    message ? h('div', { class: 'banner warn' }, message) : null,
    error,
    field('Admin token', input),
    h('button', { class: 'primary', type: 'submit' }, 'Sign in')));
  input.focus();
}

async function bootstrap() {
  const [languages, settings, status] = await Promise.all([
    api('GET', '/languages'), api('GET', '/settings'), api('GET', '/status'),
  ]);
  state.languages = languages;
  state.settings = settings;
  document.getElementById('version').textContent = `v${status.version} · rules v${status.rules_version}`;
  document.getElementById('nav').hidden = false;
  document.getElementById('logout').hidden = false;
}

const routes = [
  [/^#\/rules\/new$/, () => ruleEditor(null)],
  [/^#\/rules\/(.+)$/, m => ruleEditor(decodeURIComponent(m[1]))],
  [/^#\/groups\/new$/, () => groupEditor(null)],
  [/^#\/groups\/(.+)$/, m => groupEditor(decodeURIComponent(m[1]))],
  [/^#\/groups$/, groupList],
  [/^#\/tests\/new(?:\?rule=(.+))?$/, m => testEditor(null, m[1] ? decodeURIComponent(m[1]) : null)],
  [/^#\/tests\/(.+)$/, m => testEditor(decodeURIComponent(m[1]))],
  [/^#\/tests$/, () => testList()],
  [/^#\/config\/new$/, () => configEditor(null)],
  [/^#\/config\/(.+)$/, m => configEditor(decodeURIComponent(m[1]))],
  [/^#\/config$/, configList],
  [/^#\/status$/, statusView],
  [/^#\/rules$/, ruleList],
];

async function route() {
  if (!state.token) return showLogin();
  const hash = location.hash || '#/rules';
  const section = hash.split('/')[1];
  for (const a of document.querySelectorAll('#nav a')) a.classList.toggle('active', a.dataset.view === section);
  for (const [pattern, handler] of routes) {
    const m = hash.match(pattern);
    if (m) {
      render(h('div', { class: 'empty' }, 'Loading…'));
      try {
        await handler(m);
      } catch (err) {
        if (err.status !== 401) render(errorBanner(err));
      }
      return;
    }
  }
  location.hash = '#/rules';
}

function pageHead(title, ...actions) {
  return h('div', { class: 'page-head' }, h('h1', {}, title), h('div', { class: 'actions' }, actions));
}

function table(headers, rows, emptyText) {
  if (!rows.length) return h('div', { class: 'card empty' }, emptyText);
  return h('div', { class: 'table-wrap' }, h('table', {},
    h('thead', {}, h('tr', {}, headers.map(t => h('th', {}, t)))),
    h('tbody', {}, rows)));
}

// --- rules ------------------------------------------------------------------

async function ruleList() {
  const rules = await api('GET', '/rules');
  const rows = rules.map(r => h('tr', { class: 'clickable', onclick: () => { location.hash = `#/rules/${encodeURIComponent(r.id)}`; } },
    h('td', { class: 'mono' }, r.id),
    h('td', {}, langBadge(r.language)),
    h('td', {}, badge(r.state)),
    h('td', {}, r.groups.join(', ') || h('span', { class: 'muted' }, '—')),
    h('td', {}, r.description || h('span', { class: 'muted' }, '—')),
    h('td', { class: 'small muted' }, r.stats ? `${r.stats.evaluations} evals · ${r.stats.failures} fail · ${r.stats.errors} err · ${r.stats.mean_micros.toFixed(1)} µs` : '')));
  render(
    pageHead('Rules', h('button', { class: 'primary', onclick: () => { location.hash = '#/rules/new'; } }, 'New rule')),
    table(['Id', 'Language', 'State', 'Groups', 'Description', 'Statistics'], rows, 'No rules yet. Create the first one.'));
}

function codeEditor(language, source, onInput) {
  const gutter = h('pre', { class: 'gutter', 'aria-hidden': 'true' });
  const area = h('textarea', { spellcheck: 'false', autocapitalize: 'off', autocomplete: 'off', 'aria-label': 'Rule source' });
  area.value = source;
  const wrap = h('div', { class: `editor ${language}` }, gutter, area);
  const sync = () => {
    const lines = area.value.split('\n').length;
    gutter.textContent = Array.from({ length: lines }, (_, i) => i + 1).join('\n');
    gutter.scrollTop = area.scrollTop;
  };
  area.addEventListener('input', () => { sync(); onInput(area.value); });
  area.addEventListener('scroll', () => { gutter.scrollTop = area.scrollTop; });
  area.addEventListener('keydown', e => {
    if (e.key === 'Tab' && !e.shiftKey) {
      e.preventDefault();
      const indent = wrap.classList.contains('starlark') ? '    ' : '  ';
      const { selectionStart: s, selectionEnd: end } = area;
      area.setRangeText(indent, s, end, 'end');
      area.dispatchEvent(new Event('input'));
    }
  });
  sync();
  return {
    el: wrap,
    get value() { return area.value; },
    set value(v) { area.value = v; sync(); },
    setLanguage(lang) { wrap.className = `editor ${lang}`; },
  };
}

async function ruleEditor(id) {
  const isNew = id === null;
  const [rule, groups] = await Promise.all([
    isNew ? Promise.resolve(null) : api('GET', `/rules/${encodeURIComponent(id)}`),
    api('GET', '/groups'),
  ]);
  const defaultLang = state.languages[0] ? state.languages[0].id : 'cel';
  const lang = rule ? rule.language : defaultLang;
  const template = l => (state.languages.find(x => x.id === l) || {}).template || '';

  const idInput = h('input', { type: 'text', value: rule ? rule.id : '', readonly: !isNew, placeholder: 'e.g. customer.adult' });
  const langSelect = h('select', {}, state.languages.map(l => h('option', { value: l.id }, l.name)));
  langSelect.value = lang;
  const description = h('input', { type: 'text', value: rule ? rule.description : '' });
  const message = h('input', { type: 'text', value: rule && rule.message ? rule.message : '', placeholder: 'Used when the rule fails without a message' });
  const enabled = h('input', { type: 'checkbox', checked: rule ? rule.enabled : true });
  const groupChecks = groups.map(g => {
    const box = h('input', { type: 'checkbox', value: g.name, checked: rule ? rule.groups.includes(g.name) : false });
    return h('label', { class: 'checkbox' }, box, g.name);
  });
  const help = h('div', { class: 'help' });
  const compileStatus = h('div', { class: 'compile muted' }, 'Not checked yet.');
  const formError = h('div');

  const showHelp = l => {
    const info = state.languages.find(x => x.id === l);
    help.replaceChildren(h('strong', {}, info ? info.name : l), ' — ', info ? info.help : '');
  };
  const checkCompile = debounce(async () => {
    try {
      const r = await api('POST', '/rules/evaluate', { language: langSelect.value, source: editor.value, records: [] });
      if (r.compiled) {
        compileStatus.className = 'compile ok';
        compileStatus.replaceChildren('✓ Compiles');
      } else {
        compileStatus.className = 'compile fail';
        compileStatus.replaceChildren('✗ Does not compile', h('pre', { class: 'block' }, r.compile_error));
      }
    } catch (err) {
      compileStatus.className = 'compile fail';
      compileStatus.replaceChildren(err.message);
    }
  }, 400);
  const editor = codeEditor(lang, rule ? rule.source : template(lang), checkCompile);
  langSelect.addEventListener('change', () => {
    const previous = state.languages.map(l => l.template);
    if (!editor.value.trim() || previous.includes(editor.value)) editor.value = template(langSelect.value);
    editor.setLanguage(langSelect.value);
    showHelp(langSelect.value);
    checkCompile();
  });
  showHelp(lang);
  checkCompile();

  // Try-it panel.
  const sample = h('textarea', { rows: 6 });
  sample.value = '[\n  {"age": 30, "email": "ada@example.com"}\n]';
  const tryOut = h('div');
  const runTry = async () => {
    tryOut.replaceChildren();
    try {
      let records = parseJson(sample.value, 'Sample records');
      if (!Array.isArray(records)) records = [records];
      const r = await api('POST', '/rules/evaluate', { language: langSelect.value, source: editor.value, records });
      if (!r.compiled) {
        tryOut.append(h('div', { class: 'banner error' }, 'The rule does not compile.', h('pre', { class: 'block' }, r.compile_error)));
        return;
      }
      for (const res of r.results) {
        tryOut.append(h('div', { class: `result ${res.status === 'pass' ? 'passed' : 'failed'}` },
          h('div', { class: 'row' }, h('strong', {}, `Record ${res.index}`), badge(res.status)),
          res.message ? h('div', {}, res.message) : null,
          res.error ? h('pre', { class: 'block' }, `${res.error.kind}: ${res.error.message}`) : null));
      }
    } catch (err) {
      tryOut.append(errorBanner(err));
    }
  };

  const save = async e => {
    e.preventDefault();
    formError.replaceChildren();
    const body = {
      id: idInput.value.trim(),
      language: langSelect.value,
      source: editor.value,
      description: description.value,
      message: message.value || null,
      enabled: enabled.checked,
      metadata: rule ? rule.metadata || {} : {},
      groups: groupChecks.map(l => l.querySelector('input')).filter(b => b.checked).map(b => b.value),
    };
    try {
      const result = isNew
        ? await api('POST', '/rules', body)
        : await api('PUT', `/rules/${encodeURIComponent(id)}`, body);
      notifyApplied(result, `Rule ${body.id}`);
      if (isNew) {
        location.hash = `#/rules/${encodeURIComponent(body.id)}`;
        offerTests(body.id);
      } else {
        route();
      }
    } catch (err) {
      formError.replaceChildren(errorBanner(err));
      window.scrollTo({ top: 0, behavior: 'smooth' });
    }
  };
  const remove = async () => {
    if (!confirm(`Delete rule ${id}? It is also removed from its groups.`)) return;
    try {
      const result = await api('DELETE', `/rules/${encodeURIComponent(id)}`);
      for (const w of result.warnings || []) toast(w, 'error');
      toast(`Rule ${id} deleted.`);
      location.hash = '#/rules';
    } catch (err) {
      formError.replaceChildren(errorBanner(err));
    }
  };

  const status = rule ? h('div', { class: 'row' }, badge(rule.state),
    rule.stats ? h('span', { class: 'muted small' }, `${rule.stats.evaluations} evaluations · ${rule.stats.failures} failures · ${rule.stats.errors} errors · mean ${rule.stats.mean_micros.toFixed(1)} µs`) : null) : null;

  render(
    pageHead(isNew ? 'New rule' : `Rule ${id}`,
      !isNew ? h('button', { onclick: () => { location.hash = `#/tests/new?rule=${encodeURIComponent(id)}`; } }, 'Add test') : null,
      !isNew ? h('button', { class: 'danger', onclick: remove }, 'Delete') : null),
    formError,
    rule && rule.compile_error ? h('div', { class: 'banner error' }, 'The stored rule does not compile:', h('pre', { class: 'block' }, rule.compile_error)) : null,
    h('div', { class: 'split' },
      h('form', { class: 'card', onsubmit: save },
        status,
        h('div', { class: 'grid' },
          field('Id', idInput, isNew ? 'Letters, digits, _ . : - (cannot be changed later)' : null),
          field('Language', langSelect)),
        field('Description', description),
        field('Failure message', message),
        h('div', { class: 'field' }, h('label', { class: 'checkbox' }, enabled, 'Enabled (selected by checks)')),
        h('div', { class: 'field' }, h('label', {}, 'Groups'), groups.length ? h('div', { class: 'checks' }, groupChecks) : h('div', { class: 'muted small' }, 'No groups defined.')),
        h('div', { class: 'field' }, h('label', {}, 'Source'), help, h('div', { class: 'gap' }), editor.el, compileStatus),
        h('div', { class: 'row' }, h('button', { class: 'primary', type: 'submit' }, isNew ? 'Create rule' : 'Save changes'),
          h('a', { href: '#/rules' }, 'Cancel'))),
      h('div', { class: 'card stack' },
        h('h2', { class: 'mt0' }, 'Try it'),
        h('p', { class: 'muted small' }, 'Evaluate the current source (unsaved) against sample records using the live rule configuration.'),
        sample,
        h('button', { onclick: runTry, type: 'button' }, 'Evaluate'),
        tryOut)));
}

// After creating a rule: offer to write regression tests for it right away.
function offerTests(ruleId) {
  const rows = h('div');
  const addRow = (record, outcome) => {
    const text = h('textarea', { rows: 3 });
    text.value = record;
    const select = h('select', {}, h('option', { value: 'valid' }, 'passes (valid)'), h('option', { value: 'invalid' }, 'fails (invalid)'), h('option', { value: 'error' }, 'errors'));
    select.value = outcome;
    const row = h('div', { class: 'test-row' }, text, select, h('button', { type: 'button', class: 'ghost', onclick: () => row.remove() }, 'Remove'));
    rows.append(row);
  };
  addRow('{"age": 30}', 'valid');
  addRow('{"age": 12}', 'invalid');
  const out = h('div');
  const dialog = h('dialog', {},
    h('h1', {}, `Test rule ${ruleId} now?`),
    h('p', { class: 'muted' }, 'Add JSON records and the outcome you expect. They are saved as regression tests (scope: this rule) and run immediately.'),
    rows,
    h('div', { class: 'row' },
      h('button', { type: 'button', onclick: () => addRow('{}', 'valid') }, 'Add record'),
      h('span', { class: 'spacer' }),
      h('button', { type: 'button', class: 'ghost', onclick: () => dialog.close() }, 'Not now'),
      h('button', { type: 'button', class: 'primary', onclick: saveAndRun }, 'Save tests and run')),
    out);
  async function saveAndRun() {
    out.replaceChildren();
    const ids = [];
    try {
      const existing = new Set((await api('GET', '/tests')).map(t => t.id));
      let n = 1;
      for (const row of rows.querySelectorAll('.test-row')) {
        const record = parseJson(row.querySelector('textarea').value, `Record ${n}`);
        const outcome = row.querySelector('select').value;
        let testId;
        do { testId = `${ruleId}.case-${n++}`; } while (existing.has(testId));
        existing.add(testId);
        await api('POST', '/tests', {
          id: testId,
          description: `Created with rule ${ruleId}`,
          tags: [],
          record,
          scope: { type: 'rule', rule: ruleId },
          expect: outcome === 'valid' ? { outcome } : { outcome, rules: [ruleId] },
        });
        ids.push(testId);
      }
      const report = await api('POST', '/tests/run', { ids });
      out.append(runSummary(report));
      report.results.forEach(r => out.append(testResultCard(r)));
    } catch (err) {
      out.append(errorBanner(err));
      if (ids.length) out.append(h('p', { class: 'muted small' }, `Saved before the error: ${ids.join(', ')}`));
    }
  }
  document.body.append(dialog);
  dialog.addEventListener('close', () => dialog.remove());
  dialog.showModal();
}

// --- groups -----------------------------------------------------------------

async function groupList() {
  const groups = await api('GET', '/groups');
  const rows = groups.map(g => h('tr', { class: 'clickable', onclick: () => { location.hash = `#/groups/${encodeURIComponent(g.name)}`; } },
    h('td', { class: 'mono' }, g.name),
    h('td', {}, g.description || h('span', { class: 'muted' }, '—')),
    h('td', {}, String(g.rules.length)),
    h('td', { class: 'mono small' }, g.rules.join(', '))));
  render(
    pageHead('Rule groups', h('button', { class: 'primary', onclick: () => { location.hash = '#/groups/new'; } }, 'New group')),
    table(['Name', 'Description', 'Rules', 'Members'], rows, 'No groups yet. Checks without a group run every enabled rule.'));
}

async function groupEditor(name) {
  const isNew = name === null;
  const [group, rules] = await Promise.all([
    isNew ? Promise.resolve(null) : api('GET', `/groups/${encodeURIComponent(name)}`),
    api('GET', '/rules'),
  ]);
  const nameInput = h('input', { type: 'text', value: group ? group.name : '', readonly: !isNew });
  const description = h('input', { type: 'text', value: group ? group.description : '' });
  const members = new Set(group ? group.rules : []);
  const checks = rules.map(r => h('label', { class: 'checkbox' },
    h('input', { type: 'checkbox', value: r.id, checked: members.has(r.id) }), r.id, ' ', langBadge(r.language),
    r.enabled ? null : badge('disabled')));
  const missing = group ? group.rules.filter(id => !rules.some(r => r.id === id)) : [];
  const formError = h('div');
  const save = async e => {
    e.preventDefault();
    const selected = checks.map(l => l.querySelector('input')).filter(b => b.checked).map(b => b.value);
    const body = { name: nameInput.value.trim(), description: description.value, rules: selected };
    try {
      const result = isNew ? await api('POST', '/groups', body) : await api('PUT', `/groups/${encodeURIComponent(name)}`, body);
      notifyApplied(result, `Group ${body.name}`);
      location.hash = '#/groups';
    } catch (err) {
      formError.replaceChildren(errorBanner(err));
    }
  };
  const remove = async () => {
    if (!confirm(`Delete group ${name}? Its rules are kept.`)) return;
    try {
      await api('DELETE', `/groups/${encodeURIComponent(name)}`);
      toast(`Group ${name} deleted.`);
      location.hash = '#/groups';
    } catch (err) {
      formError.replaceChildren(errorBanner(err));
    }
  };
  render(
    pageHead(isNew ? 'New group' : `Group ${name}`, !isNew ? h('button', { class: 'danger', onclick: remove }, 'Delete') : null),
    formError,
    missing.length ? h('div', { class: 'banner warn' }, `References unknown rules (ignored by checks): ${missing.join(', ')}`) : null,
    h('form', { class: 'card', onsubmit: save },
      h('div', { class: 'grid' }, field('Name', nameInput), field('Description', description)),
      h('div', { class: 'field' }, h('label', {}, 'Rules'), rules.length ? h('div', { class: 'checks' }, checks) : h('div', { class: 'muted' }, 'No rules defined.')),
      h('div', { class: 'row' }, h('button', { class: 'primary', type: 'submit' }, isNew ? 'Create group' : 'Save changes'), h('a', { href: '#/groups' }, 'Cancel'))));
}

// --- tests ------------------------------------------------------------------

function describeScope(scope) {
  if (!scope || scope.type === 'all') return 'all rules';
  if (scope.type === 'group') return `group ${scope.group}`;
  return `rule ${scope.rule}`;
}

function describeExpect(expect) {
  let text = `expect ${expect.outcome}`;
  if (expect.rules && expect.rules.length) text += ` by ${expect.rules.join(', ')}${expect.exclusive ? ' only' : ''}`;
  return text;
}

function runSummary(report) {
  const kind = report.passed === report.total ? 'ok' : 'error';
  return h('div', { class: `banner ${kind}` },
    `${report.total} test(s): ${report.passed} passed, ${report.failed} failed, ${report.errors} errors · ${report.duration_ms.toFixed(1)} ms · rules v${report.rules_version}`);
}

function testResultCard(r) {
  return h('div', { class: `result ${r.status}` },
    h('div', { class: 'row' }, h('strong', { class: 'mono' }, r.id), badge(r.status), h('span', { class: 'muted small' }, `${describeScope(r.scope)} · ${describeExpect(r.expected)}`)),
    r.actual ? h('div', { class: 'small' }, 'Actual: ', badge(r.actual.outcome),
      r.actual.failed_rules.length ? ` failed: ${r.actual.failed_rules.join(', ')}` : '',
      r.actual.errored_rules.length ? ` errored: ${r.actual.errored_rules.join(', ')}` : '') : null,
    r.diagnostics.length ? h('ul', {}, r.diagnostics.map(d => h('li', {}, d))) : null,
    r.rules.length ? h('details', {}, h('summary', { class: 'small muted' }, 'Rule outcomes'),
      h('ul', {}, r.rules.map(o => h('li', {}, h('span', { class: 'mono' }, o.rule), ' ', badge(o.status),
        o.message ? ` ${o.message}` : '', o.error ? ` ${o.error.kind}: ${o.error.message}` : '')))) : null);
}

async function testList(report) {
  const tests = await api('GET', '/tests');
  const output = h('div');
  if (report) output.append(runSummary(report), ...report.results.filter(r => r.status !== 'passed').map(testResultCard));
  const selected = new Set();
  const run = async ids => {
    output.replaceChildren(h('div', { class: 'muted' }, 'Running…'));
    try {
      await testList(await api('POST', '/tests/run', ids ? { ids } : {}));
    } catch (err) {
      output.replaceChildren(errorBanner(err));
    }
  };
  const rows = tests.map(t => {
    const box = h('input', { type: 'checkbox', onclick: e => { e.stopPropagation(); box.checked ? selected.add(t.id) : selected.delete(t.id); } });
    return h('tr', { class: 'clickable', onclick: () => { location.hash = `#/tests/${encodeURIComponent(t.id)}`; } },
      h('td', { onclick: e => e.stopPropagation() }, box),
      h('td', { class: 'mono' }, t.id),
      h('td', {}, describeScope(t.scope)),
      h('td', {}, describeExpect(t.expect)),
      h('td', {}, t.tags.join(', ')),
      h('td', {}, t.latest ? badge(t.latest.status) : h('span', { class: 'muted' }, 'not run')),
      h('td', { class: 'small' }, t.latest && t.latest.diagnostics.length ? t.latest.diagnostics[0] : ''));
  });
  render(
    pageHead('Tests',
      h('button', { onclick: () => selected.size ? run([...selected]) : toast('Select tests first.') }, 'Run selected'),
      h('button', { onclick: () => run(null) }, 'Run all'),
      h('button', { class: 'primary', onclick: () => { location.hash = '#/tests/new'; } }, 'New test')),
    output,
    table(['', 'Id', 'Scope', 'Expectation', 'Tags', 'Last result', 'Diagnostics'], rows, 'No tests yet.'));
}

async function testEditor(id, forRule) {
  const isNew = id === null;
  const [test, rules, groups] = await Promise.all([
    isNew ? Promise.resolve(null) : api('GET', `/tests/${encodeURIComponent(id)}`),
    api('GET', '/rules'),
    api('GET', '/groups'),
  ]);
  const scope = test ? test.scope : forRule ? { type: 'rule', rule: forRule } : { type: 'all' };
  const expect = test ? test.expect : { outcome: forRule ? 'invalid' : 'valid', rules: forRule ? [forRule] : [] };

  const idInput = h('input', { type: 'text', value: test ? test.id : forRule ? `${forRule}.case` : '', readonly: !isNew });
  const description = h('input', { type: 'text', value: test ? test.description : '' });
  const tags = h('input', { type: 'text', value: test ? test.tags.join(', ') : '', placeholder: 'comma-separated' });
  const scopeType = h('select', {}, h('option', { value: 'all' }, 'All enabled rules'), h('option', { value: 'group' }, 'A rule group'), h('option', { value: 'rule' }, 'A single rule'));
  scopeType.value = scope.type;
  const scopeGroup = h('select', {}, groups.map(g => h('option', { value: g.name }, g.name)));
  if (scope.group) scopeGroup.value = scope.group;
  const scopeRule = h('select', {}, rules.map(r => h('option', { value: r.id }, r.id)));
  if (scope.rule) scopeRule.value = scope.rule;
  const scopeTarget = h('div');
  const record = h('textarea', { rows: 10 });
  record.value = fmtJson(test ? test.record : {});
  const outcome = h('select', {}, h('option', { value: 'valid' }, 'valid — every rule passes'), h('option', { value: 'invalid' }, 'invalid — at least one rule fails'), h('option', { value: 'error' }, 'error — a rule errors'));
  outcome.value = expect.outcome;
  const expectRules = h('input', { type: 'text', value: (expect.rules || []).join(', '), placeholder: 'rule ids (optional)' });
  const exclusive = h('input', { type: 'checkbox', checked: !!expect.exclusive });
  const formError = h('div');
  const output = h('div');

  const updateScope = () => {
    scopeTarget.replaceChildren(scopeType.value === 'group' ? field('Group', scopeGroup) : scopeType.value === 'rule' ? field('Rule', scopeRule) : '');
  };
  scopeType.addEventListener('change', updateScope);
  updateScope();

  const body = () => {
    const s = scopeType.value === 'group' ? { type: 'group', group: scopeGroup.value }
      : scopeType.value === 'rule' ? { type: 'rule', rule: scopeRule.value } : { type: 'all' };
    const list = expectRules.value.split(',').map(x => x.trim()).filter(Boolean);
    return {
      id: idInput.value.trim(),
      description: description.value,
      tags: tags.value.split(',').map(x => x.trim()).filter(Boolean),
      record: parseJson(record.value, 'The record'),
      scope: s,
      expect: { outcome: outcome.value, rules: outcome.value === 'valid' ? [] : list, exclusive: outcome.value !== 'valid' && exclusive.checked },
    };
  };
  const save = async e => {
    e.preventDefault();
    formError.replaceChildren();
    try {
      const b = body();
      const result = isNew ? await api('POST', '/tests', b) : await api('PUT', `/tests/${encodeURIComponent(id)}`, b);
      toast(`Test ${b.id} saved.`);
      if (result && isNew) location.hash = `#/tests/${encodeURIComponent(b.id)}`;
      else route();
    } catch (err) {
      formError.replaceChildren(errorBanner(err));
    }
  };
  const run = async () => {
    output.replaceChildren();
    try {
      const report = await api('POST', `/tests/${encodeURIComponent(id)}/run`);
      output.append(...report.results.map(testResultCard));
    } catch (err) {
      output.append(errorBanner(err));
    }
  };
  const remove = async () => {
    if (!confirm(`Delete test ${id}?`)) return;
    try {
      await api('DELETE', `/tests/${encodeURIComponent(id)}`);
      toast(`Test ${id} deleted.`);
      location.hash = '#/tests';
    } catch (err) {
      formError.replaceChildren(errorBanner(err));
    }
  };
  if (test && test.latest) output.append(testResultCard(test.latest));
  render(
    pageHead(isNew ? 'New test' : `Test ${id}`,
      !isNew ? h('button', { onclick: run }, 'Run') : null,
      !isNew ? h('button', { class: 'danger', onclick: remove }, 'Delete') : null),
    formError,
    h('div', { class: 'split' },
      h('form', { class: 'card', onsubmit: save },
        h('div', { class: 'grid' }, field('Id', idInput), field('Tags', tags, 'Run a suite by tag from the API: {"tags": ["..."]}')),
        field('Description', description),
        h('div', { class: 'grid' }, field('Scope', scopeType, 'Which rules evaluate the record'), scopeTarget),
        field('Record (JSON object)', record),
        h('div', { class: 'grid' },
          field('Expected outcome', outcome),
          field('Rules that must fail / error', expectRules, 'Optional, for invalid and error outcomes')),
        h('div', { class: 'field' }, h('label', { class: 'checkbox' }, exclusive, 'Only these rules may fail / error')),
        h('div', { class: 'row' }, h('button', { class: 'primary', type: 'submit' }, isNew ? 'Create test' : 'Save changes'), h('a', { href: '#/tests' }, 'Cancel'))),
      h('div', { class: 'card' }, h('h2', { class: 'mt0' }, 'Latest result'), output.childNodes.length ? output : h('div', {}, output, h('p', { class: 'muted' }, isNew ? 'Save the test to run it.' : 'Not run yet.')))));
}

// --- configuration ----------------------------------------------------------

async function configList() {
  const entries = await api('GET', '/config');
  const rows = entries.map(e => h('tr', { class: 'clickable', onclick: () => { location.hash = `#/config/${encodeURIComponent(e.key)}`; } },
    h('td', { class: 'mono' }, e.key),
    h('td', { class: 'mono small' }, JSON.stringify(e.value)),
    h('td', {}, e.exposed_to_rules ? badge('rule — visible to rules', 'rule-scope') : badge('system')),
    h('td', {}, e.apply === 'restart' ? badge('restart required', 'restart') : e.apply === 'hot' ? badge('live', 'ok') : ''),
    h('td', {}, e.description)));
  const known = state.settings.map(s => h('tr', {},
    h('td', { class: 'mono' }, s.key),
    h('td', { class: 'small' }, s.type === 'enum' ? s.values.join(' | ') : s.type === 'integer' ? (s.max === null ? `integer ≥ ${s.min}` : `integer ${s.min}…${s.max}`) : s.type),
    h('td', { class: 'mono small' }, s.default === null ? 'computed' : JSON.stringify(s.default)),
    h('td', {}, s.apply === 'restart' ? badge('restart', 'restart') : badge('live', 'ok')),
    h('td', { class: 'small' }, s.description)));
  render(
    pageHead('Configuration', h('button', { class: 'primary', onclick: () => { location.hash = '#/config/new'; } }, 'New entry')),
    h('p', { class: 'muted' }, 'Entries with scope "rule" are exposed to rules as the config variable. "system" entries are Drover settings and are never visible to rules.'),
    table(['Key', 'Value', 'Scope', 'Applies', 'Description'], rows, 'No configuration entries: every setting uses its default.'),
    h('h2', {}, 'Known system settings'),
    table(['Key', 'Type', 'Default', 'Applies', 'Description'], known, ''));
}

async function configEditor(key) {
  const isNew = key === null;
  const entry = isNew ? null : await api('GET', `/config/${encodeURIComponent(key)}`);
  const keyInput = h('input', { type: 'text', value: entry ? entry.key : '', readonly: !isNew, list: 'known-settings' });
  const datalist = h('datalist', { id: 'known-settings' }, state.settings.map(s => h('option', { value: s.key })));
  const scope = h('select', {}, h('option', { value: 'rule' }, 'rule — visible to rules'), h('option', { value: 'system' }, 'system — Drover setting'));
  scope.value = entry ? entry.scope : 'rule';
  const value = h('textarea', { rows: 5 });
  value.value = entry ? fmtJson(entry.value) : '';
  const description = h('input', { type: 'text', value: entry ? entry.description : '' });
  const settingHelp = h('div', { class: 'help' });
  const formError = h('div');
  const updateHelp = () => {
    const spec = state.settings.find(s => s.key === keyInput.value.trim());
    if (scope.value === 'system' && spec) {
      const type = spec.type === 'enum' ? `one of ${spec.values.map(v => JSON.stringify(v)).join(', ')}` : spec.type === 'integer' ? (spec.max === null ? `an integer of at least ${spec.min}` : `an integer between ${spec.min} and ${spec.max}`) : 'true or false';
      settingHelp.hidden = false;
      settingHelp.replaceChildren(h('strong', {}, spec.key), `: ${spec.description} Value: ${type}. `, spec.apply === 'restart' ? badge('restart required', 'restart') : badge('applies live', 'ok'));
    } else if (scope.value === 'system') {
      settingHelp.hidden = false;
      settingHelp.replaceChildren('System entries must use a known setting key.');
    } else {
      settingHelp.hidden = false;
      settingHelp.replaceChildren('Rules read this value as ', h('code', {}, `config.${keyInput.value.trim() || 'key'}`), ' (CEL) or ', h('code', {}, `config["${keyInput.value.trim() || 'key'}"]`), ' (Starlark).');
    }
  };
  keyInput.addEventListener('input', () => {
    if (isNew && state.settings.some(s => s.key === keyInput.value.trim())) scope.value = 'system';
    updateHelp();
  });
  scope.addEventListener('change', updateHelp);
  updateHelp();
  const save = async e => {
    e.preventDefault();
    formError.replaceChildren();
    try {
      const body = { key: keyInput.value.trim(), scope: scope.value, value: parseJson(value.value, 'The value'), description: description.value };
      const result = isNew ? await api('POST', '/config', body) : await api('PUT', `/config/${encodeURIComponent(key)}`, body);
      notifyApplied(result, `Entry ${body.key}`);
      location.hash = '#/config';
    } catch (err) {
      formError.replaceChildren(errorBanner(err));
    }
  };
  const remove = async () => {
    if (!confirm(`Delete configuration entry ${key}?`)) return;
    try {
      const result = await api('DELETE', `/config/${encodeURIComponent(key)}`);
      toast(result.restart_required ? `Entry ${key} deleted; takes effect after a restart.` : `Entry ${key} deleted.`);
      location.hash = '#/config';
    } catch (err) {
      formError.replaceChildren(errorBanner(err));
    }
  };
  render(
    pageHead(isNew ? 'New configuration entry' : `Configuration ${key}`, !isNew ? h('button', { class: 'danger', onclick: remove }, 'Delete') : null),
    formError,
    h('form', { class: 'card', onsubmit: save },
      h('div', { class: 'grid' }, field('Key', h('div', {}, keyInput, datalist)), field('Scope', scope)),
      settingHelp,
      h('div', { class: 'gap' }),
      field('Value (JSON)', value, 'For example 18, "strict", true, ["IT", "FR"] or {"limit": 5}'),
      field('Description', description),
      h('div', { class: 'row' }, h('button', { class: 'primary', type: 'submit' }, isNew ? 'Create entry' : 'Save changes'), h('a', { href: '#/config' }, 'Cancel'))));
}

// --- status -----------------------------------------------------------------

async function statusView() {
  const s = await api('GET', '/status');
  const kv = pairs => h('dl', { class: 'kv' }, pairs.map(([k, v]) => [h('dt', {}, k), h('dd', {}, v)]));
  const refresh = async () => {
    try {
      const r = await api('POST', '/refresh');
      toast(r.changed ? `Rules reloaded (version ${r.version}).` : 'No changes in the data sources.');
      statusView();
    } catch (err) {
      toast(err.message, 'error');
    }
  };
  const startupTests = s.startup.tests;
  render(
    pageHead('Status', h('button', { onclick: refresh }, 'Reload from data sources')),
    s.refresh.last_error ? h('div', { class: 'banner error' }, 'Last refresh failed: ', s.refresh.last_error) : null,
    s.invalid_rules.length ? h('div', { class: 'banner error' }, `${s.invalid_rules.length} rule(s) do not compile: `, s.invalid_rules.map(r => r.id).join(', ')) : null,
    h('div', { class: 'grid' },
      h('div', { class: 'card' }, h('h2', { class: 'mt0' }, 'Rules'), kv([
        ['Snapshot version', String(s.rules_version)],
        ['Loaded at', new Date(s.loaded_at).toLocaleString()],
        ['Rules', `${s.rules} (${s.enabled_rules} enabled)`],
        ['Groups', String(s.groups)],
        ['Rule config keys', s.rule_config_keys.join(', ') || '—'],
        ['Languages', s.languages.join(', ')],
      ])),
      h('div', { class: 'card' }, h('h2', { class: 'mt0' }, 'Execution'), kv([
        ['Worker threads', `${s.pool.threads} (${s.pool.busy} busy, ${s.pool.queued} queued)`],
        ['Admitted checks', `${s.checks.admitted} / ${s.checks.capacity}`],
        ['Unknown group mode', s.settings.unknown_group_mode],
        ['Max records per check', String(s.settings.max_records_per_check)],
      ])),
      h('div', { class: 'card' }, h('h2', { class: 'mt0' }, 'Cache refresh'), kv([
        ['Last success', s.refresh.last_success ? new Date(s.refresh.last_success).toLocaleString() : '—'],
        ['Last change', s.refresh.last_change ? new Date(s.refresh.last_change).toLocaleString() : '—'],
        ['Refreshes', `${s.refresh.refreshes} (${s.refresh.changes} changes, ${s.refresh.failures} failures)`],
        ['Polling', s.settings.refresh_interval ? `every ${s.settings.refresh_interval.secs} s` : 'disabled'],
      ])),
      h('div', { class: 'card' }, h('h2', { class: 'mt0' }, 'Startup'), kv([
        ['Started', new Date(s.startup.started_at).toLocaleString()],
        ['Startup time', `${s.startup.duration_ms.toFixed(0)} ms`],
        ['Startup tests', startupTests ? `${startupTests.passed}/${startupTests.total} passed` : 'not run'],
        ['Recovered executions', String(s.startup.recovered_executions)],
      ]))),
    h('h2', {}, 'Data sources'),
    h('div', { class: 'card' }, kv(Object.entries(s.data_sources))),
    s.diagnostics.length || s.startup.warnings.length ? h('h2', {}, 'Diagnostics') : null,
    s.diagnostics.length || s.startup.warnings.length ? h('div', { class: 'card' }, h('ul', {}, [...new Set([...s.diagnostics, ...s.startup.warnings])].map(d => h('li', {}, d)))) : null);
}

// --- start ------------------------------------------------------------------

window.addEventListener('hashchange', route);
window.addEventListener('DOMContentLoaded', async () => {
  document.getElementById('logout').addEventListener('click', () => signOut());
  if (!state.token) return showLogin();
  try {
    await bootstrap();
    route();
  } catch (err) {
    if (err.status !== 401) showLogin(err.message);
  }
});
