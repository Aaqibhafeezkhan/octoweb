const assert = require('node:assert/strict');
const vm = require('node:vm');

function checkSearch(html) {
  const nodes = new Map(), messages = [], timers = new Map();
  let timerId = 0;
  class Element {
    constructor() {
      this.value = ''; this.style = {}; this.listeners = new Map(); this.children = [];
      const classes = new Set();
      this.classList = {
        add: (...names) => names.forEach(name => classes.add(name)),
        remove: (...names) => names.forEach(name => classes.delete(name)),
        contains: name => classes.has(name),
        toggle: (name, on) => { if (on) classes.add(name); else classes.delete(name); },
      };
    }
    set innerHTML(value) {
      this.markup = value;
      this.children.forEach(id => nodes.delete(id));
      this.children = Array.from(value.matchAll(/\bid="([^"]+)"/g), match => match[1]);
      this.children.forEach(id => nodes.set(id, new Element()));
    }
    get innerHTML() { return this.markup; }
    addEventListener(type, listener) {
      if (!this.listeners.has(type)) this.listeners.set(type, []);
      this.listeners.get(type).push(listener);
    }
    fire(type) { this.listeners.get(type).forEach(listener => listener({ target: this })); }
    querySelector(selector) {
      if (selector === '.ic-enter' || selector === '.ic-exit') return new Element();
      return null;
    }
    querySelectorAll() { return []; }
    focus() { context.document.activeElement = this; }
  }
  const start = html.indexOf('<script>') + '<script>'.length;
  for (const match of html.slice(0, start).matchAll(/\bid="([^"]+)"/g)) nodes.set(match[1], new Element());
  const context = {
    document: { body: new Element(), getElementById: id => nodes.get(id), addEventListener() {} },
    window: { ipc: { postMessage: json => messages.push(JSON.parse(json)) }, addEventListener() {} },
    matchMedia: () => ({ matches: true }),
    setTimeout: fn => { timers.set(++timerId, fn); return timerId; },
    clearTimeout: id => timers.delete(id),
  };
  vm.runInNewContext(html.slice(start, html.indexOf('</script>', start)), context);
  const mail = context.window.__mail;
  const search = nodes.get('search'), list = nodes.get('list');
  search.tagName = 'INPUT';
  const last = () => messages[messages.length - 1];
  function setup(ws) { mail.setup({ ws, ai: true, accounts: [{ key: 'acc', address: 'me@example.com' }] }); }
  function type(query) {
    search.value = query; search.fire('input');
    for (const [id, fn] of timers) { timers.delete(id); fn(); }
    mail.search({ query, hits: [] });
  }
  function ask() { nodes.get('ask-ai').fire('click'); return last(); }
  function ready(request, query = 'from:Anna invoice', ws = 'work') {
    mail.searchReady({ ws, id: request.id, query });
  }

  setup('work'); type('the invoice Anna sent');
  const first = ask();
  assert.equal(first.type, 'search_ai');
  assert.equal(first.query, 'the invoice Anna sent');
  assert.match(list.innerHTML, /Searching…/);
  assert.match(list.innerHTML, /id="ask-ai"[^>]* disabled/);
  assert.match(list.innerHTML, /Finding mail…/);
  const count = messages.length;
  ready(first, 'from:Wrong', 'other');
  ready({ id: first.id + 1 });
  assert.equal(messages.length, count);
  ready(first);
  assert.equal(search.value, 'from:Anna invoice');
  assert.deepEqual(last(), { type: 'search', query: 'from:Anna invoice', server: true });
  ready(first, 'from:Duplicate');
  assert.equal(search.value, 'from:Anna invoice');
  mail.search({ query: search.value, hits: [] });
  assert.doesNotMatch(list.innerHTML, /Searching…/);

  type('invoice');
  const failed = ask();
  mail.searchFailed({ ws: 'work', id: failed.id, message: 'Agent <offline>' });
  assert.match(list.innerHTML, /role="alert">Agent &lt;offline&gt;/);
  const retry = ask();
  assert.ok(retry.id > failed.id);
  mail.searchFailed({ ws: 'work', id: failed.id, message: 'Old failure' });
  assert.doesNotMatch(list.innerHTML, /Old failure/);
  ready(failed, 'from:Old');
  assert.equal(search.value, 'invoice');
  ready(retry);

  type('invoice');
  const edited = ask();
  search.value = 'a different question'; search.fire('input');
  ready(edited);
  assert.equal(search.value, 'a different question');

  type('invoice');
  const entered = ask();
  search.focus();
  mail.key({ key: 'Enter', shift: false });
  assert.deepEqual(last(), { type: 'search', query: 'invoice', server: true });
  ready(entered);
  assert.equal(search.value, 'invoice');
  nodes.get('back-inbox').fire('click');
  ready(entered);
  assert.equal(search.value, '');

  type('invoice');
  const switched = ask();
  setup('other'); setup('work');
  ready(switched);
  assert.equal(search.value, '');
  type('invoice');
  const newer = ask();
  assert.ok(newer.id > switched.id);
  ready(switched);
  assert.equal(search.value, 'invoice');
  ready(newer);
  assert.equal(search.value, 'from:Anna invoice');
  assert.ok(messages.every(message => message.type !== 'ai'));
}
