const assert = require('node:assert/strict');
const vm = require('node:vm');

function specialistFixture(html, source) {
  class Element {
    constructor() {
      this.children = []; this.dataset = {}; this.className = ''; this.textContent = ''; this.innerHTML = '';
      this.open = false;
      this.classList = {
        contains: name => this.className.split(' ').includes(name),
        add: name => { if (!this.classList.contains(name)) this.className += ' ' + name; },
        toggle: (name, on) => {
          this.className = this.className.split(' ').filter(part => part !== name).join(' ');
          if (on) this.classList.add(name);
        },
      };
    }
    get parentNode() { return this.parent || null; }
    appendChild(child) { this.children.push(child); child.parent = this; }
    remove() { this.parent.children.splice(this.parent.children.indexOf(this), 1); this.parent = null; }
    replaceChildren() { this.children = []; }
    replaceWith(child) {
      const index = this.parent.children.indexOf(this);
      this.parent.children[index] = child; child.parent = this.parent; this.parent = null;
    }
    addEventListener() {}
    querySelectorAll(selector) {
      const matches = [];
      for (const child of this.children) {
        if (selector === '[data-log-index]' ? child.dataset.logIndex !== undefined : child.classList.contains(selector.slice(1))) matches.push(child);
        matches.push(...child.querySelectorAll(selector));
      }
      return matches;
    }
    querySelector(selector) { return this.querySelectorAll(selector)[0]; }
  }
  const escapeHtml = text => String(text).replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;');
  const parent = { currentAgentRaw: 'parent text', currentAgentBubble: {}, busy: true, isThinking: true,
    toolRows: { same: 'parent tool' }, toolDetails: [], msgQueue: ['next prompt'], runningTools: 1 };
  const s = { ...parent, sid: 1, runViews: new Map(), log: [], container: new Element(), renderedFrom: 0 };
  const context = { document: { createElement: () => new Element() }, window: {}, sessions: new Map([[1, s]]),
    activeSid: 1, escapeHtml, renderMd: escapeHtml,
    mountMsg: (state, el) => { el.dataset.logIndex = state.log.length; state.container.appendChild(el); },
    logPush: (state, record) => state.log.push(record), scrollToBottom() {}, syncMoreRow() {},
    KIND_ICONS: {}, kindIcon: () => '', ICON_X_CIRCLE: 'fail', ICON_CHECK: 'check', fmtDuration: ms => String(ms),
  };
  function extract(start, end) {
    assert.notEqual(html.indexOf(start), -1); assert.notEqual(html.indexOf(end), -1);
    return html.slice(html.indexOf(start), html.indexOf(end, html.indexOf(start)));
  }
  vm.createContext(context);
  vm.runInContext(extract('  function formatJson(', '  // ── Steps group'), context);
  vm.runInContext(extract('  function buildGroupItem(', '  function tickActivity('), context);
  vm.runInContext(source, context);
  const run = { run_id: 'tap-general-abcdef', role: 'developer:<general>', status: 'running', seq: 1,
    truncated: false, error: null, complete: true, events: [{ kind: 'status', status: 'running', error: null }] };
  context.window.__setRun(1, run);
  const view = s.runViews.get(run.run_id);
  function delta(event, options = {}, evicted = []) {
    const update = { run_id: run.run_id, role: run.role, status: run.status, seq: run.seq + 1,
      truncated: run.truncated, event, merged: false, dropped: 0, ...options };
    context.window.__updateRun(1, { kind: 'delta', delta: update }, evicted);
  }
  return { s, parent, context, run, view, delta };
}

function checkSpecialist(html, source) {
  const { s, parent, context, run, view, delta } = specialistFixture(html, source);
  assert.equal(s.container.children.length, 1);
  assert.match(view.summary.innerHTML, /developer:&lt;general&gt;/);
  assert.match(view.summary.innerHTML, /tap-general-abcdef/);
  assert.match(view.summary.innerHTML, /running/);
  assert.match(view.summary.innerHTML, /specialist-report-chevron/);
  assert.equal(view.details.open, false, 'a run starts collapsed, like a report');
  delta({ kind: 'prompt', content: '<input>' });
  delta({ kind: 'thinking', content: '<thought>' });
  delta({ kind: 'assistant', content: 'child <reply>' });
  delta({ kind: 'tool_use', tool: 'read', tool_id: 'same', params: { path: '<file>' } });
  assert.match(view.summary.innerHTML, /specialist-report-preview">read</);
  assert.equal(view.details.open, false);
  assert.match(view.content.querySelector('.specialist-report-content').innerHTML, /child &lt;reply&gt;/);
  assert.equal(view.content.querySelector('.specialist-report-input-text').textContent, '<input>');
  assert.equal(view.content.querySelector('.tool-row').classList.contains('running'), true);
  const item = view.content.querySelector('.tool-item'); item.classList.add('expanded');
  delta({ kind: 'tool_result', tool_id: 'same', content: '<result>', success: null });
  delta({ kind: 'usage', cost: 0.1, input_tokens: 10, output_tokens: 20 });
  delta({ kind: 'usage', cost: 0.2, input_tokens: 30, output_tokens: 40 });
  assert.equal(view.content.querySelector('.tool-item').classList.contains('expanded'), true);
  assert.match(view.content.querySelector('.tool-detail').innerHTML, /&lt;result&gt;/);
  assert.doesNotMatch(view.content.querySelector('.tool-row').innerHTML, /tool-check|tool-fail/);
  assert.equal(view.content.children.filter(el => el.textContent.startsWith('$')).length, 1);
  assert.match(view.content.children.at(-1).textContent, /0.2000 · 30 input · 40 output/);
  delta({ kind: 'tool_use', tool: 'fail', tool_id: 'bad', params: {} });
  delta({ kind: 'tool_result', tool_id: 'bad', content: 'error', success: false });
  delta({ kind: 'status', status: 'failed', error: 'child failure' }, { status: 'failed' });
  context.window.__updateRun(1, { kind: 'error', run_id: run.run_id, error: 'Replay failed: unavailable' }, []);
  assert.match(view.content.querySelectorAll('.tool-row').at(-1).innerHTML, /tool-fail/);
  assert.equal(view.details.classList.contains('failed'), true);
  assert.match(view.error.textContent, /Replay failed/);
  assert.equal(s.log.length, 1); assert.equal(s.log[0].role, 'run');
  for (const [key, value] of Object.entries(parent)) assert.equal(s[key], value, 'parent ' + key + ' changed');
  context.window.__setRun(1, { ...run, run_id: 'tap-other-123456', role: null });
  assert.equal(s.runViews.size, 2); assert.equal(s.log.length, 2);
  // Replay re-renders a run where it was, not below newer messages.
  const first = s.runViews.get(run.run_id).wrap;
  context.window.__setRun(1, run);
  assert.equal(s.container.children.length, 2);
  assert.notEqual(s.container.children[0], first);
  assert.equal(s.container.children[0], s.runViews.get(run.run_id).wrap);
  assert.equal(s.log.length, 2);
  s.runViews.clear(); s.container.replaceChildren(); context.renderRun(s, run);
  assert.equal(s.container.children.length, 1);
  assert.match(s.runViews.get(run.run_id).summary.innerHTML, /failed/);
}

function checkSpecialistDeltas(html, source) {
  const { s, context, run, view, delta } = specialistFixture(html, source);
  delta({ kind: 'prompt', content: 'saved prompt' });
  const input = view.content.children[0]; input.open = true;
  delta({ kind: 'assistant', content: 'hello ' });
  const text = view.content.querySelector('.specialist-report-content');
  delta({ kind: 'assistant', content: 'world' }, { merged: true });
  assert.equal(view.content.querySelector('.specialist-report-content'), text);
  assert.equal(text.innerHTML, 'hello world');
  assert.equal(view.content.children[0], input); assert.equal(input.open, true);
  context.window.__updateRun(1, { kind: 'recovering', run_id: run.run_id }, []);
  assert.equal(view.content.querySelector('.specialist-report-content'), text);
  assert.equal(text.innerHTML, 'hello world'); assert.equal(run.complete, false);
  assert.match(view.summary.innerHTML, /recovering/);

  delta({ kind: 'tool_use', tool: 'read', tool_id: 'call', params: { large: 'trim me' } });
  delta({ kind: 'tool_result', tool_id: 'call', content: 'retained result', success: true });
  const result = view.tools.get('call');
  delta({ kind: 'usage', cost: 1, input_tokens: 10, output_tokens: 20 }, { dropped: 4, truncated: true });
  assert.equal(run.events.length, 2); assert.equal(view.entries.length, 2);
  assert.equal(result.tool.rawInput, null); assert.equal(result.tool.title, 'call');
  assert.equal(view.content.querySelector('.specialist-report-content'), undefined);
  assert.match(view.trimmed.textContent, /Earlier steps trimmed/);
  assert.match(view.content.querySelector('.tool-row').innerHTML, /tool-check/);
  // Oversized chunks arrive as trim-only deltas, never as huge payloads.
  delta(null, { dropped: 2, truncated: true });
  assert.equal(run.events.length, 0); assert.equal(view.entries.length, 0);
  assert.equal(view.tools.size, 0); assert.equal(view.content.children.length, 0);
  assert.equal(view.usage, null);
  context.window.__setRun(1, { ...run, run_id: 'other', events: [] });
  delta({ kind: 'status', status: 'done', error: null }, { status: 'done' }, [run.run_id]);
  assert.equal(s.runViews.has(run.run_id), false);
  assert.equal(s.log.some(record => record.run.run_id === run.run_id), false);
  assert.equal(s.container.children.length, 1);
  assert.equal(s.runViews.get('other').wrap.dataset.logIndex, 0);
  // Re-entering a workspace also removes runs evicted while its sidebar was hidden.
  context.window.__replayRuns(1, []);
  assert.equal(s.log.length, 0); assert.equal(s.runViews.size, 0);
  assert.equal(s.container.children.length, 0);
}
