// Program list and program editor.

import { api } from '../lib/api.js';
import { el } from '../lib/dom.js';
import { programPath, scaleMax } from '../lib/chart.js';
import { chartGrid } from '../lib/chart-dom.js';
import {
  parseDuration, formatDuration, expand, summary, toProgram, copyName, MAX_REPEAT, MAX_NAME,
} from '../lib/program-edit.js';
import { loadSelected } from '../lib/selection.js';
import { setBeforeLeave } from '../lib/nav.js';
import { openStartDialog } from './start-dialog.js';

const SVG_NS = 'http://www.w3.org/2000/svg';
const CHART_W = 1000;
const CHART_H = 240;
const MAX_STEP_NAME = 40;

function svg(tag, attrs = {}) {
  const node = document.createElementNS(SVG_NS, tag);
  for (const [k, v] of Object.entries(attrs)) node.setAttribute(k, v);
  return node;
}

// ---------------------------------------------------------------- list

export const list = {
  mount(root) {
    let programs = [];
    let busy = false;
    const error = el('p', { class: 'error', role: 'alert', hidden: true });
    const hint = el('p', { class: 'muted hint', hidden: true },
      'Zum Fahren zuerst unter ', el('a', { href: '#/riders', class: 'link' }, 'Training'), ' eine Person wählen.');
    const body = el('div', { class: 'choice-list' });
    root.append(el('section', { class: 'page programs' },
      el('div', { class: 'page-head' },
        el('h1', {}, 'Programme'),
        el('a', { class: 'btn primary', href: '#/programs/new' }, 'Neues Programm')),
      error, hint, body));

    const showError = (msg) => { error.textContent = msg || ''; error.hidden = !msg; };

    async function run(fn) {
      if (busy) return;
      busy = true;
      showError('');
      try {
        await fn();
      } catch (e) {
        showError(e.message);
      } finally {
        busy = false;
      }
    }

    function drive(p) {
      const person = loadSelected();
      if (person == null) { location.hash = '#/riders'; return; }
      openStartDialog(p, person);
    }

    function copy(p) {
      return run(async () => {
        const { id } = await api('/api/programs', { method: 'POST', body: { ...p, id: null, name: copyName(p.name) } });
        location.hash = `#/programs/${id}`;
      });
    }

    function remove(p) {
      if (!confirm(`Programm „${p.name}“ wirklich löschen?`)) return;
      return run(async () => {
        await api(`/api/programs/${p.id}`, { method: 'DELETE' });
        programs = programs.filter((x) => x.id !== p.id);
        render();
      });
    }

    function render() {
      hint.hidden = !(programs.length > 0 && loadSelected() == null);
      if (programs.length === 0) {
        body.replaceChildren(el('p', { class: 'muted' }, 'Noch kein Programm angelegt.'));
        return;
      }
      body.replaceChildren(...programs.map((p) => el('div', { class: 'program-row' },
        el('div', { class: 'program-info' },
          el('span', { class: 'choice-name' }, p.name),
          el('span', { class: 'choice-meta' }, formatDuration(summary(p).totalSecs))),
        el('div', { class: 'row-actions' },
          el('button', { type: 'button', class: 'btn primary', onclick: () => drive(p) }, 'Fahren'),
          el('a', { class: 'btn', href: `#/programs/${p.id}` }, 'Bearbeiten'),
          el('button', { type: 'button', class: 'btn', onclick: () => copy(p) }, 'Kopieren'),
          el('button', { type: 'button', class: 'btn danger', onclick: () => remove(p) }, 'Löschen')))));
    }

    (async () => {
      try {
        programs = await api('/api/programs');
        render();
      } catch (e) {
        showError(e.message);
      }
    })();
    return () => {};
  },
};

// ---------------------------------------------------------------- editor

const newStep = (name = '', dur = '', from = '', to = '') => ({ name, dur, from, to });
const newBlock = (repeat = '1', steps = [newStep()]) => ({ repeat, steps });
const blankDraft = () => ({ id: null, name: '', blocks: [newBlock()] });

function fromProgram(p) {
  return {
    id: p.id ?? null,
    name: p.name,
    blocks: p.blocks.map((b) => newBlock(String(b.repeat), b.steps.map((s) =>
      newStep(s.name, formatDuration(s.duration_secs), String(s.start_watts), String(s.end_watts))))),
  };
}

const wholeNumber = (text) => (/^\d+$/.test(String(text).trim()) ? Number(text) : null);

// Draft backup in sessionStorage, so a reload or accidental navigation keeps the form.
const draftKey = (id) => `kettler.program_draft.${id == null ? 'new' : id}`;

function isDraft(d) {
  return d && typeof d.name === 'string' && Array.isArray(d.blocks) && d.blocks.every((b) =>
    b && typeof b.repeat === 'string' && Array.isArray(b.steps) && b.steps.every((s) =>
      s && ['name', 'dur', 'from', 'to'].every((k) => typeof s[k] === 'string')));
}

function loadBackup(id) {
  try {
    const text = sessionStorage.getItem(draftKey(id));
    const d = text == null ? null : JSON.parse(text);
    return isDraft(d) ? d : null;
  } catch {
    return null;
  }
}

function saveBackup(id, draft) {
  try {
    sessionStorage.setItem(draftKey(id), JSON.stringify(draft));
  } catch {
    // Storage unavailable: no backup.
  }
}

function clearBackup(id) {
  try {
    sessionStorage.removeItem(draftKey(id));
  } catch {
    // Storage unavailable.
  }
}

function field(label, input) {
  return el('label', { class: 'field' }, el('span', { class: 'lbl' }, label), input);
}

function textInput(value, label, oninput, attrs = {}) {
  const input = el('input', { type: 'text', value, autocomplete: 'off', 'aria-label': label, ...attrs });
  input.addEventListener('input', () => oninput(input.value));
  return input;
}

export const edit = {
  mount(root, params) {
    const id = params && params[0] != null ? Number(params[0]) : null;
    let limits = { min: 25, max: 400 };
    let draft = blankDraft();
    let base = JSON.stringify(draft);
    let saved = base;
    let saving = false;
    let ready = false;
    let disposed = false;

    const error = el('p', { class: 'error', role: 'alert', hidden: true });
    const note = el('div', { class: 'banner info restored', hidden: true },
      el('span', {}, 'Ungespeicherter Entwurf wiederhergestellt'),
      el('button', { type: 'button', class: 'btn', onclick: () => discardDraft() }, 'Verwerfen'));
    const nameInput = textInput('', 'Programmname', (v) => { draft.name = v; refresh(); }, { maxlength: String(MAX_NAME), placeholder: 'Programmname' });
    const blocksBox = el('div', { class: 'blocks' });
    const templatePanel = el('div', { class: 'panel template', hidden: true });
    const previewBox = el('div', { class: 'tile preview' });
    const saveBtn = el('button', { type: 'button', class: 'btn primary', onclick: () => save() }, 'Speichern');
    const cancelBtn = el('button', { type: 'button', class: 'btn', onclick: () => { location.hash = '#/programs'; } }, 'Abbrechen');
    const total = el('span', { class: 'choice-meta' });
    const form = el('div', { class: 'editor-form' });

    const dirty = () => JSON.stringify(draft) !== saved;
    const showError = (msg) => { error.textContent = msg || ''; error.hidden = !msg; };

    function setBusy(on) {
      for (const c of form.querySelectorAll('input, button')) c.disabled = on;
      form.classList.toggle('disabled', on);
    }

    // Leaving with changes asks first; the app keeps the screen when this returns false.
    setBeforeLeave(() => {
      if (!dirty()) return true;
      if (!confirm('Änderungen verwerfen?')) return false;
      clearBackup(id);
      return true;
    });

    function persist() {
      if (!ready) return;
      if (dirty()) saveBackup(id, draft);
      else clearBackup(id);
    }

    function discardDraft() {
      draft = JSON.parse(base);
      saved = base;
      clearBackup(id);
      note.hidden = true;
      nameInput.value = draft.name;
      templatePanel.hidden = true;
      showError('');
      renderBlocks();
    }

    function renderPreview() {
      const res = toProgram(draft, limits);
      if (!res.program) {
        previewBox.replaceChildren(el('span', { class: 'lbl' }, 'Vorschau'),
          el('p', { class: 'muted' }, 'Die Vorschau erscheint, sobald alle Angaben gültig sind.'));
        total.textContent = '';
        return;
      }
      const segments = expand(res.program);
      const secs = summary(res.program).totalSecs;
      const max = scaleMax(segments, []);
      const area = svg('path', { class: 'program-area', d: programPath(segments, secs, CHART_W, CHART_H, max) });
      const chart = svg('svg', { viewBox: `0 0 ${CHART_W} ${CHART_H}`, preserveAspectRatio: 'none', class: 'chart-svg' });
      const grid = chartGrid(CHART_W, CHART_H);
      grid.update(max);
      chart.append(grid.lines, area);
      total.textContent = `Gesamtdauer ${formatDuration(secs)}`;
      previewBox.replaceChildren(
        el('div', { class: 'chart-head' }, el('span', { class: 'lbl' }, 'Vorschau'),
          el('span', { class: 'chart-time' }, `${formatDuration(secs)} · max. ${max} W`)),
        el('div', { class: 'chart-body preview-body' }, chart, grid.labels));
    }

    function refresh() {
      renderPreview();
      persist();
    }

    function stepRow(block, step) {
      const dur = textInput(step.dur, 'Dauer', (v) => { step.dur = v; refresh(); }, { placeholder: '2:00' });
      dur.addEventListener('blur', () => {
        const secs = parseDuration(dur.value);
        if (secs != null && dur.value !== formatDuration(secs)) { dur.value = formatDuration(secs); step.dur = dur.value; refresh(); }
      });
      return el('div', { class: 'step-row' },
        field('Name', textInput(step.name, 'Name des Abschnitts', (v) => { step.name = v; refresh(); }, { maxlength: String(MAX_STEP_NAME) })),
        field('Dauer', dur),
        field('Watt von', textInput(step.from, 'Watt von', (v) => { step.from = v; refresh(); }, { inputmode: 'numeric', pattern: '[0-9]*' })),
        field('Watt bis', textInput(step.to, 'Watt bis', (v) => { step.to = v; refresh(); }, { inputmode: 'numeric', pattern: '[0-9]*' })),
        el('button', {
          type: 'button', class: 'btn icon', 'aria-label': 'Abschnitt entfernen',
          onclick: () => { block.steps = block.steps.filter((s) => s !== step); renderBlocks(); },
        }, '✕'));
    }

    function renderBlocks() {
      blocksBox.replaceChildren(...draft.blocks.map((block, i) => el('section', { class: 'panel block' },
        el('div', { class: 'block-head' },
          el('h2', {}, `Block ${i + 1}`),
          el('label', { class: 'repeat' }, 'Wiederholen ×',
            textInput(block.repeat, `Wiederholungen Block ${i + 1}`, (v) => { block.repeat = v; refresh(); }, { inputmode: 'numeric', pattern: '[0-9]*' })),
          el('button', {
            type: 'button', class: 'btn', onclick: () => {
              if (!confirm(`Block ${i + 1} entfernen?`)) return;
              draft.blocks = draft.blocks.filter((b) => b !== block);
              renderBlocks();
            },
          }, 'Block entfernen')),
        ...block.steps.map((s) => stepRow(block, s)),
        el('button', { type: 'button', class: 'btn', onclick: () => { block.steps.push(newStep()); renderBlocks(); } }, 'Abschnitt hinzufügen'))));
      refresh();
    }

    function openTemplate() {
      const n = textInput('5', 'Anzahl', () => {}, { inputmode: 'numeric', pattern: '[0-9]*' });
      const hardDur = textInput('2:00', 'Dauer hart', () => {});
      const hardW = textInput('150', 'Watt hart', () => {}, { inputmode: 'numeric', pattern: '[0-9]*' });
      const easyDur = textInput('2:00', 'Dauer locker', () => {});
      const easyW = textInput('80', 'Watt locker', () => {}, { inputmode: 'numeric', pattern: '[0-9]*' });
      const tplError = el('p', { class: 'error', role: 'alert', hidden: true });
      templatePanel.hidden = false;
      templatePanel.replaceChildren(
        el('h2', {}, 'Intervall-Block'), tplError,
        el('div', { class: 'tpl-grid' },
          field('Wiederholungen n', n),
          field('Dauer hart', hardDur), field('Watt hart', hardW),
          field('Dauer locker', easyDur), field('Watt locker', easyW)),
        el('div', { class: 'row-actions' },
          el('button', {
            type: 'button', class: 'btn primary', onclick: () => {
              const repeat = wholeNumber(n.value);
              const hd = parseDuration(hardDur.value);
              const ed = parseDuration(easyDur.value);
              const hw = wholeNumber(hardW.value);
              const ew = wholeNumber(easyW.value);
              let msg = '';
              if (repeat == null || repeat < 1 || repeat > MAX_REPEAT) msg = `Wiederholungen müssen zwischen 1 und ${MAX_REPEAT} liegen`;
              else if (hd == null || ed == null || hd < 1 || ed < 1) msg = 'Dauer ungültig, z. B. 2:00, 90 oder 10 min';
              else if (hw == null || ew == null || hw < limits.min || hw > limits.max || ew < limits.min || ew > limits.max) {
                msg = `Watt müssen zwischen ${limits.min} und ${limits.max} liegen`;
              }
              if (msg) { tplError.textContent = msg; tplError.hidden = false; return; }
              draft.blocks.push(newBlock(String(repeat), [
                newStep('Hart', formatDuration(hd), String(hw), String(hw)),
                newStep('Locker', formatDuration(ed), String(ew), String(ew))]));
              templatePanel.hidden = true;
              renderBlocks();
            },
          }, 'Block anlegen'),
          el('button', { type: 'button', class: 'btn', onclick: () => { templatePanel.hidden = true; } }, 'Abbrechen')));
    }

    async function save() {
      if (saving) return;
      showError('');
      const res = toProgram(draft, limits);
      if (!res.program) { showError(res.error); return; }
      saving = true;
      saveBtn.disabled = true;
      try {
        await api('/api/programs', { method: 'POST', body: res.program });
        saved = JSON.stringify(draft);
        clearBackup(id);
        location.hash = '#/programs';
      } catch (e) {
        showError(e.message);
      } finally {
        saving = false;
        saveBtn.disabled = false;
      }
    }

    form.append(
      el('div', { class: 'editor-name' }, field('Name', nameInput), total),
      blocksBox,
      el('div', { class: 'row-actions' },
        el('button', { type: 'button', class: 'btn', onclick: () => { draft.blocks.push(newBlock()); renderBlocks(); } }, 'Block hinzufügen'),
        el('button', { type: 'button', class: 'btn', onclick: openTemplate }, 'Intervall-Block')),
      templatePanel,
      previewBox,
      el('div', { class: 'row-actions editor-actions' },
        saveBtn,
        cancelBtn));

    root.append(el('section', { class: 'page program-editor' },
      el('h1', {}, id == null ? 'Neues Programm' : 'Programm bearbeiten'),
      error, note, form));

    renderBlocks();
    setBusy(true);

    (async () => {
      const [lim, program] = await Promise.all([
        api('/api/limits').catch(() => null), // the built-in range is used; the server still has the last word
        id == null ? null : api(`/api/programs/${id}`).then((r) => r.program).catch((e) => e),
      ]);
      if (disposed) return;
      if (lim) limits = { min: lim.min, max: lim.max };
      if (program instanceof Error) {
        showError(program.message);
        cancelBtn.disabled = false;
        return;
      }
      draft = program ? fromProgram(program) : blankDraft();
      base = JSON.stringify(draft);
      saved = base;
      const backup = loadBackup(id);
      if (backup && JSON.stringify(backup) !== base) {
        draft = { ...backup, id: draft.id };
        note.hidden = false;
      }
      nameInput.value = draft.name;
      renderBlocks();
      setBusy(false);
      ready = true;
      refresh();
    })();

    return () => { disposed = true; };
  },
};
