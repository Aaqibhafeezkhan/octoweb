  function runHeader(view) {
    const run = view.run;
    view.details.classList.toggle('failed', run.status === 'failed' || run.error !== null);
    const who = run.role === null ? 'Specialist' : run.role;
    view.summary.innerHTML = '<span class="specialist-report-who">' + escapeHtml(who) + '</span>' +
      '<span class="specialist-report-title">run · <span class="specialist-report-status">' + escapeHtml(run.status + (!run.complete && run.error === null ? ' · recovering' : '')) + '</span></span>' +
      '<span class="specialist-report-preview">' + escapeHtml(run.run_id) + '</span>';
    view.trimmed.textContent = run.truncated ? 'Earlier steps trimmed.' : '';
    view.error.textContent = run.error === null ? '' : run.error;
  }

  function runToolRow(view, entry) {
    const item = buildGroupItem(entry.tool, view.run.status === 'running');
    if (entry.node && entry.node.classList.contains('expanded')) {
      item.classList.add('expanded');
      item.querySelector('.tool-detail').innerHTML = renderToolDetail(entry.tool);
    }
    if (entry.node) entry.node.replaceWith(item);
    else view.content.appendChild(item);
    entry.node = item;
  }

  function runText(entry) {
    if (entry.event.kind === 'assistant') entry.text.innerHTML = renderMd(entry.event.content);
    else entry.text.textContent = entry.event.content;
  }

  function appendRunEntry(view, event) {
    const entry = { event, node: null, result: null };
    view.entries.push(entry);
    switch (event.kind) {
      case 'prompt':
      case 'thinking': {
        entry.node = document.createElement('details');
        entry.node.className = 'specialist-report-input';
        const summary = document.createElement('summary');
        summary.textContent = event.kind === 'prompt' ? 'Input' : 'Thinking';
        entry.text = document.createElement('div');
        entry.text.className = 'specialist-report-input-text';
        entry.node.appendChild(summary);
        entry.node.appendChild(entry.text);
        runText(entry);
        break;
      }
      case 'assistant':
        entry.node = document.createElement('div');
        entry.node.className = 'specialist-report-content';
        entry.text = entry.node;
        runText(entry);
        break;
      case 'tool_use':
        entry.tool = { kind: 'other', title: event.tool, status: 'running', duration: null,
          rawInput: event.params, rawOutput: null, locations: [] };
        view.tools.set(event.tool_id, entry);
        runToolRow(view, entry);
        return;
      case 'tool_result': {
        const source = view.tools.get(event.tool_id);
        entry.tool = { kind: 'other', title: source ? source.tool.title : event.tool_id,
          status: event.success === true ? 'completed' : event.success === false ? 'failed' : 'unknown',
          duration: null, rawInput: source ? source.tool.rawInput : null, rawOutput: event.content, locations: [] };
        if (source) {
          source.result = entry;
          entry.node = source.node;
          source.node = null;
        }
        view.tools.set(event.tool_id, entry);
        runToolRow(view, entry);
        return;
      }
      case 'usage':
        if (view.usage) { view.usage.node.remove(); view.usage.node = null; }
        entry.node = document.createElement('div');
        entry.node.className = 'tool-detail-title';
        entry.node.textContent = '$' + event.cost.toFixed(4) + ' · ' + event.input_tokens + ' input · ' + event.output_tokens + ' output tokens';
        view.usage = entry;
        break;
      case 'status':
        if (event.error !== null) {
          entry.node = document.createElement('div');
          entry.node.className = 'tool-detail-title';
          entry.node.textContent = event.error;
        }
        if (event.status !== 'running') {
          for (const tool of view.tools.values()) {
            if (tool.tool.status === 'running') runToolRow(view, tool);
          }
        }
        break;
      default: throw new Error('Unknown specialist event: ' + event.kind);
    }
    if (entry.node) view.content.appendChild(entry.node);
  }

  function trimRunEntries(view, count) {
    for (const entry of view.entries.splice(0, count)) {
      if (entry.node) entry.node.remove();
      if (entry.result) {
        // A retained result must not keep a trimmed call's parameters alive.
        entry.result.tool.rawInput = null;
        entry.result.tool.title = entry.result.event.tool_id;
        runToolRow(view, entry.result);
      }
      if (view.tools.get(entry.event.tool_id) === entry) view.tools.delete(entry.event.tool_id);
      if (view.usage === entry) view.usage = null;
    }
    view.run.events.splice(0, count);
  }

  // Full construction is reserved for replay/remount; live deltas update only
  // their own text segment or tool row, preserving other DOM and disclosure state.
  function renderRun(s, run) {
    const previous = s.runViews.get(run.run_id);
    if (previous) previous.wrap.remove();
    const wrap = document.createElement('div');
    wrap.className = 'msg agent specialist specialist-report';
    const details = document.createElement('details');
    details.className = 'specialist-report-details';
    details.open = previous ? previous.details.open : true;
    const summary = document.createElement('summary');
    summary.className = 'specialist-report-summary';
    const body = document.createElement('div');
    body.className = 'msg-bubble specialist-report-body';
    const trimmed = document.createElement('div');
    trimmed.className = 'tool-detail-title';
    const content = document.createElement('div');
    const error = document.createElement('div');
    error.className = 'tool-detail-title';
    body.appendChild(trimmed); body.appendChild(content); body.appendChild(error);
    details.appendChild(summary); details.appendChild(body); wrap.appendChild(details);
    mountMsg(s, wrap);
    const view = { run, wrap, details, summary, content, trimmed, error, entries: [], tools: new Map(), usage: null };
    s.runViews.set(run.run_id, view);
    run.events.forEach(event => appendRunEntry(view, event));
    runHeader(view);
    return view;
  }

  function recordRun(s, view) {
    const at = s.log.findIndex(record => record.role === 'run' && record.run.run_id === view.run.run_id);
    if (at >= 0) {
      s.log[at].run = view.run;
      view.wrap.dataset.logIndex = at;
    } else logPush(s, { role: 'run', text: '', run: view.run });
  }

  window.__setRun = function(sid, run) {
    const s = sessions.get(sid);
    if (!s) return;
    recordRun(s, renderRun(s, run));
    if (s.sid === activeSid) scrollToBottom();
  };

  function evictRun(s, id) {
    const view = s.runViews.get(id);
    if (view) { view.wrap.remove(); s.runViews.delete(id); }
    const index = s.log.findIndex(record => record.role === 'run' && record.run.run_id === id);
    if (index >= 0) {
      s.log.splice(index, 1);
      if (index < s.renderedFrom) s.renderedFrom--;
      for (const node of s.container.querySelectorAll('[data-log-index]')) {
        if (Number(node.dataset.logIndex) > index) node.dataset.logIndex = Number(node.dataset.logIndex) - 1;
      }
      syncMoreRow(s);
    }
  }

  window.__replayRuns = function(sid, runs) {
    const s = sessions.get(sid);
    if (!s) return;
    const retained = new Set(runs.map(run => run.run_id));
    const previous = new Set([...s.runViews.keys(), ...s.log.filter(record => record.role === 'run').map(record => record.run.run_id)]);
    for (const id of previous) if (!retained.has(id)) evictRun(s, id);
    runs.forEach(run => window.__setRun(sid, run));
  };

  window.__updateRun = function(sid, update, evicted) {
    const s = sessions.get(sid);
    if (!s) return;
    if (update.kind === 'snapshot') {
      window.__setRun(sid, update.run);
    } else {
      const id = update.kind === 'delta' ? update.delta.run_id : update.run_id;
      let view = s.runViews.get(id);
      if (!view) {
        const record = s.log.find(record => record.role === 'run' && record.run.run_id === id);
        const run = record ? record.run : { run_id: id, role: null, status: 'unknown', seq: 0,
          truncated: false, error: null, complete: false, events: [] };
        view = renderRun(s, run);
        recordRun(s, view);
      }
      if (update.kind === 'delta') {
        const delta = update.delta;
        const run = view.run;
        run.role = delta.role; run.status = delta.status; run.seq = delta.seq;
        run.truncated = delta.truncated; run.complete = true;
        if (delta.event !== null) {
          if (delta.merged) {
            const entry = view.entries[view.entries.length - 1];
            entry.event.content += delta.event.content;
            runText(entry);
          } else {
            run.events.push(delta.event);
            appendRunEntry(view, delta.event);
          }
        }
        trimRunEntries(view, delta.dropped);
      } else if (update.kind === 'recovering') {
        view.run.complete = false;
        view.run.error = null;
      } else if (update.kind === 'error') {
        view.run.error = update.error;
        view.run.complete = false;
      } else throw new Error('Unknown specialist update: ' + update.kind);
      runHeader(view);
    }
    evicted.forEach(id => evictRun(s, id));
    if (s.sid === activeSid) scrollToBottom();
  };
