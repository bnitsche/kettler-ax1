// Rider selection, then program or free ride.

import { api } from '../lib/api.js';
import * as live from '../lib/live.js';
import { mmss } from '../lib/format.js';
import { el } from '../lib/dom.js';
import { loadSelected, saveSelected, clearSelected } from '../lib/selection.js';
import { personDeleteText } from '../lib/delete-text.js';
import { parseProfile } from '../lib/profile.js';
import { expectRide } from './ride.js';
import { openStartDialog } from './start-dialog.js';
import { openConfirmDialog } from './confirm-dialog.js';

function programSecs(program) {
  let total = 0;
  for (const b of program.blocks || []) {
    for (const s of b.steps || []) total += (b.repeat || 1) * s.duration_secs;
  }
  return total;
}

export function mount(root) {
  let persons = [];
  let programs = [];
  let straps = []; // registered chest straps, for the profile's choice
  let selected = loadSelected();
  let busy = false;
  let editing = false;
  let profiling = null; // person id whose row shows the profile form

  const error = el('p', { class: 'error', role: 'alert', hidden: true });
  const running = el('div', { class: 'banner info', hidden: true },
    'Es läuft bereits eine Fahrt. ', el('a', { href: '#/ride', class: 'link' }, 'Zur Fahrt'));
  const personList = el('div', { class: 'choice-list' });
  const nameInput = el('input', { type: 'text', placeholder: 'Name', maxlength: '40', autocomplete: 'off', 'aria-label': 'Name der neuen Person' });
  const addForm = el('form', { class: 'add-person' }, nameInput, el('button', { type: 'submit', class: 'btn' }, 'Neue Person'));
  const editBtn = el('button', { type: 'button', class: 'btn small', 'aria-pressed': 'false', onclick: () => { editing = !editing; profiling = null; showError(''); renderPersons(); } }, 'Bearbeiten');
  const programList = el('div', { class: 'choice-list' });
  const programSection = el('section', { class: 'panel' }, el('h2', {}, '2. Programm wählen'), programList);

  root.append(el('section', { class: 'page riders' },
    el('h1', {}, 'Training'),
    running,
    error,
    el('div', { class: 'columns' },
      el('section', { class: 'panel' }, el('div', { class: 'panel-head' }, el('h2', {}, '1. Wer fährt?'), editBtn), personList, addForm),
      programSection)));

  function showError(msg) {
    error.textContent = msg || '';
    error.hidden = !msg;
  }

  function renderPersons() {
    editBtn.textContent = editing ? 'Fertig' : 'Bearbeiten';
    editBtn.setAttribute('aria-pressed', editing ? 'true' : 'false');
    editBtn.hidden = persons.length === 0;
    personList.classList.toggle('editing', editing);
    personList.replaceChildren(...persons.map((p) => {
      const choose = el('button', {
        type: 'button',
        class: `choice${p.id === selected ? ' selected' : ''}`,
        onclick: () => { selected = p.id; saveSelected(p.id); renderPersons(); renderPrograms(); },
      }, p.name);
      if (!editing) return choose;
      if (p.id === profiling) return profileRow(p);
      return el('div', { class: 'choice-row' }, choose,
        el('button', { type: 'button', class: 'btn', 'aria-label': `Profil von ${p.name}`, onclick: () => { showError(''); profiling = p.id; renderPersons(); } }, 'Profil'),
        el('button', { type: 'button', class: 'btn danger', 'aria-label': `${p.name} löschen`, onclick: () => askDelete(p) }, 'Löschen'));
    }));
    if (persons.length === 0) personList.append(el('p', { class: 'muted' }, 'Noch keine Person angelegt.'));
    const input = personList.querySelector('.profile-name');
    if (input) { input.focus(); input.select(); }
  }

  function cancelProfile() {
    profiling = null;
    showError('');
    renderPersons();
  }

  function field(label, hint, input) {
    return el('label', { class: 'profile-field' },
      el('span', { class: 'profile-label' }, label), input,
      ...(hint ? [el('small', { class: 'muted' }, hint)] : []));
  }

  function profileRow(p) {
    const name = el('input', { type: 'text', class: 'profile-input profile-name', maxlength: '40', autocomplete: 'off', value: p.name });
    const weight = el('input', { type: 'text', class: 'profile-input', inputmode: 'decimal', autocomplete: 'off', value: String(p.weight_kg).replace('.', ',') });
    const flat = el('input', { type: 'text', class: 'profile-input', inputmode: 'numeric', autocomplete: 'off', value: String(p.flat_watts) });
    // Only offered once a strap is registered (service page, tab "Brustgurt").
    const strap = straps.length ? el('select', { class: 'profile-input' },
      el('option', { value: '' }, '– keiner –'),
      ...straps.map((s) => el('option', { value: String(s.id), selected: s.id === p.strap_id }, s.name))) : null;
    const save = el('button', { type: 'submit', class: 'btn' }, 'Speichern');
    const form = el('form', { class: 'profile-form', novalidate: true },
      field('Name', null, name),
      field('Gewicht (kg)', 'ohne Rad, 10 kg für das Rad rechnet der Server dazu', weight),
      field('Ebene (W)', 'Leistung, die für dich locker in der Ebene ist, z. B. 70 W ≈ 20 km/h', flat),
      ...(strap ? [field('Brustgurt', 'sein Puls zählt bei deinen Fahrten', strap)] : []),
      el('div', { class: 'profile-actions' }, save,
        el('button', { type: 'button', class: 'btn', onclick: cancelProfile }, 'Abbrechen')));
    form.addEventListener('keydown', (ev) => { if (ev.key === 'Escape') { ev.preventDefault(); cancelProfile(); } });
    let sending = false;
    form.addEventListener('submit', async (ev) => {
      ev.preventDefault();
      if (sending) return;
      const r = parseProfile({ name: name.value, weight: weight.value, flat: flat.value, strap: strap ? strap.value : undefined }, p);
      if (!r.ok) { showError(r.error); return; }
      showError('');
      sending = true;
      save.disabled = true;
      try {
        await api(`/api/persons/${p.id}`, { method: 'PUT', body: r.body });
        profiling = null;
        await load();
      } catch (e) {
        showError(e.message);
        if (e.status === 404) { profiling = null; load(); }
      } finally {
        sending = false;
        save.disabled = false;
      }
    });
    return form;
  }

  async function askDelete(person) {
    showError('');
    let rides;
    try {
      ({ rides } = await api(`/api/persons/${person.id}/stats`));
    } catch (e) {
      showError(e.message);
      if (e.status === 404) load();
      return;
    }
    openConfirmDialog({
      title: `${person.name} löschen`,
      text: personDeleteText(person.name, rides),
      confirmLabel: 'Löschen',
      onConfirm: async () => {
        try {
          await api(`/api/persons/${person.id}`, { method: 'DELETE' });
        } catch (e) {
          if (e.status === 404) load();
          throw e;
        }
        if (selected === person.id) { selected = null; clearSelected(); }
        await load();
      },
    });
  }

  function renderPrograms() {
    programSection.classList.toggle('disabled', selected == null);
    const free = el('button', { type: 'button', class: 'choice primary', onclick: () => startRide(null) },
      el('span', { class: 'choice-name' }, 'Frei fahren'), el('span', { class: 'choice-meta' }, 'ohne Programm'));
    const items = programs.map((p) => el('button', { type: 'button', class: 'choice', onclick: () => chooseProgram(p) },
      el('span', { class: 'choice-name' }, p.name), el('span', { class: 'choice-meta' }, mmss(programSecs(p)))));
    programList.replaceChildren(free, ...items);
    for (const b of programList.querySelectorAll('button')) b.disabled = selected == null || busy;
  }

  function chooseProgram(p) {
    if (selected == null || busy) return;
    showError('');
    openStartDialog(p, selected);
  }

  async function startRide(programId) {
    if (selected == null || busy) return;
    busy = true;
    renderPrograms();
    showError('');
    try {
      const body = { person_id: selected };
      if (programId != null) body.program_id = programId;
      const { id } = await api('/api/rides', { method: 'POST', body });
      expectRide(id);
      location.hash = '#/ride';
    } catch (e) {
      showError(e.message);
    } finally {
      busy = false;
      renderPrograms();
    }
  }

  addForm.addEventListener('submit', async (ev) => {
    ev.preventDefault();
    const name = nameInput.value.trim();
    if (!name) { showError('Bitte einen Namen eingeben'); return; }
    showError('');
    try {
      const { id } = await api('/api/persons', { method: 'POST', body: { name } });
      nameInput.value = '';
      selected = id;
      saveSelected(id);
      persons = await api('/api/persons');
      renderPersons();
      renderPrograms();
    } catch (e) {
      showError(e.message);
    }
  });

  async function load() {
    try {
      let strapList;
      [persons, programs, strapList] = await Promise.all([
        api('/api/persons'), api('/api/programs'),
        // Without straps (or if the call fails) the profile just has no strap choice.
        api('/api/straps').catch(() => ({ straps: [] })),
      ]);
      straps = strapList.straps;
      if (selected != null && !persons.some((p) => p.id === selected)) selected = null;
      renderPersons();
      renderPrograms();
    } catch (e) {
      showError(e.message);
    }
  }

  function onView(view) {
    running.hidden = !view.ride;
  }

  renderPersons();
  renderPrograms();
  load();
  const current = live.current();
  if (current) onView(current);
  const off = live.onLive(onView);
  return () => off();
}
