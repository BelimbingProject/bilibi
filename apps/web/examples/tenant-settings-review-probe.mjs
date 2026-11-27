import assert from 'node:assert/strict';
import { webcrypto } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';

const { card, html, plain, secret, value } = JSON.parse(readFileSync(0, 'utf8'));
const instance = card.instance;
const origin = new URL(instance.origin).origin;
const entry = card.entries[0];

class Element {
  constructor(tag) {
    Object.assign(this, {
      tag, children: [], events: {}, dataset: {}, disabled: false,
      style: { setProperty() {} }, classList: { add() {}, remove() {}, contains: () => false },
    });
  }
  append(...children) { this.children.push(...children); }
  prepend(...children) { this.children.unshift(...children); }
  replaceChildren(...children) { this.children = children; }
  addEventListener(name, listener) { this.events[name] = listener; }
  setAttribute(name, value) { this[name] = value; }
  focus() {}
  contains(other) { return this === other || this.children.some(child => child.contains?.(other)); }
  querySelector() { return null; }
  querySelectorAll(tags) {
    return this.children.flatMap(child => [
      ...(tags.split(',').includes(child.tag) ? [child] : []),
      ...child.querySelectorAll(tags),
    ]);
  }
}

for (const mode of ['single', 'linked', 'grid']) {
  const nodes = new Map();
  const node = id => {
    if (!nodes.has(id)) nodes.set(id, new Element(id));
    return nodes.get(id);
  };
  let receive;
  const outgoing = [];
  const parent = { postMessage: message => outgoing.push(structuredClone(message)) };
  const send = data => receive({ source: parent, origin, data: { jsonrpc: '2.0', ...data } });
  const turn = () => new Promise(resolve => setImmediate(resolve));
  const document = {
    body: node('body'), documentElement: node('html'), getElementById: node,
    createElement: tag => new Element(tag), addEventListener() {},
  };
  runInNewContext(html.match(/<script>([\s\S]*?)<\/script>/)[1], {
    window: { addEventListener: (name, listener) => { if (name === 'message') receive = listener; } },
    document, parent, crypto: webcrypto, Uint8Array, setTimeout, clearTimeout,
    setInterval() {}, ResizeObserver: class { observe() {} }, matchMedia: () => ({ matches: false }),
  });
  send({ id: outgoing.shift().id, result: { hostContext: {} } });
  await turn();
  const draft = structuredClone(card);
  if (mode === 'linked') {
    draft.linked = true;
    draft.operations = [{ key: card.key, input: entry }];
  }
  if (mode === 'grid') draft.entries.push(structuredClone(entry));
  draft.mcp_requests_per_minute = 60000;
  send({ method: 'ui/notifications/tool-result', params: { structuredContent: draft } });

  async function checkPlan(expected) {
    let plan;
    // The card checks the current draft by itself shortly after it opens or is edited.
    for (let i = 0; i < 400 && !plan; i++) {
      await new Promise(resolve => setTimeout(resolve, 5));
      const status = outgoing.findIndex(message => message.params?.name === 'review_status');
      if (status >= 0) {
        send({ id: outgoing.splice(status, 1)[0].id, result: { structuredContent: { status: 'open' } } });
      }
      plan = outgoing.find(message => message.params?.name === 'plan');
    }
    assert(plan, 'the card must still be able to run a dry run');
    const { operations } = plan.params.arguments;
    assert.deepEqual(operations.map(operation => operation.input), expected);
    assert(!JSON.stringify(outgoing).includes(secret));
    assert(!JSON.stringify(outgoing).includes(value));
    outgoing.length = 0;
    send({ id: plan.id, result: { structuredContent: {
      instance, valid: true, results: operations.map((_, index) => ({ index, valid: true })),
    } } });
    await turn();
  }

  const entries = Array.from({ length: mode === 'grid' ? 2 : 1 }, () => entry);
  await checkPlan(entries);
  const controls = () => node('editor').querySelectorAll('input,select');
  const select = controls().find(control => control.id === 'field-0-key');
  assert.equal(select.tag, 'select');
  assert(!select.querySelectorAll('option').some(option => option.textContent === secret));
  select.value = secret;
  select.events.change();
  assert.equal(controls().find(control => control.id === 'field-0-value').value, entry.value);
  if (mode !== 'linked') {
    select.events.paste({
      clipboardData: { getData: () => plain + '\t' + entry.value + '\n' + secret + '\t' + value },
      preventDefault() {},
    });
    assert.equal(node('status').dataset.tone, 'danger');
    assert.match(node('status').textContent, /available key/i);
    node('add').events.click();
    assert(!controls().some(control => control.id === 'field-' + entries.length + '-value'));
    entries.push({});
    await checkPlan(entries);
  } else {
    // Choosing an unavailable key is not an edit, so nothing is checked or sent.
    await new Promise(resolve => setTimeout(resolve, 1000));
    assert(!outgoing.some(message => message.params?.name === 'plan'));
    assert(!JSON.stringify(outgoing).includes(secret));
  }
  if (mode !== 'linked') {
    const last = entries.length - 1;
    const newKey = controls().find(control => control.id === 'field-' + last + '-key');
    newKey.value = newKey.querySelectorAll('option').find(option => option.textContent === plain).value;
    newKey.events.change();
    const newValue = controls().find(control => control.id === 'field-' + last + '-value');
    newValue.value = 'new row value';
    newValue.events.input();
    entries[last] = { key: plain, value: newValue.value };
    await checkPlan(entries);
  }
}
console.log('PASS setting-key edits, paste and new rows keep encrypted values out of the host');
