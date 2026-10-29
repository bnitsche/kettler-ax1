// Confirmation before something irreversible. `onConfirm` runs the action; if it throws,
// the message is shown in the dialog and it stays open. Closes on success.

import { el } from '../lib/dom.js';

/**
 * @param {{ title: string, text: string, confirmLabel: string, onConfirm: () => Promise<void> }} opts
 */
export function openConfirmDialog({ title, text, confirmLabel, onConfirm }) {
  const opener = document.activeElement;
  const main = document.getElementById('app');
  let busy = false;
  let closed = false;

  const error = el('p', { class: 'error', role: 'alert', hidden: true });
  const confirmBtn = el('button', { type: 'button', class: 'btn danger', onclick: () => run() }, confirmLabel);
  const cancelBtn = el('button', { type: 'button', class: 'btn', onclick: () => close() }, 'Abbrechen');
  const box = el('div', { class: 'dialog confirm-dialog', role: 'alertdialog', 'aria-modal': 'true', 'aria-label': title },
    el('h2', { class: 'confirm-title' }, title),
    el('p', { class: 'confirm-text' }, text),
    error,
    el('div', { class: 'dialog-actions' }, confirmBtn, cancelBtn));
  const backdrop = el('div', { class: 'dialog-backdrop' }, box);

  function close() {
    if (busy || closed) return;
    closed = true;
    if (main) main.inert = false;
    document.removeEventListener('keydown', onKey);
    window.removeEventListener('hashchange', onHash);
    backdrop.remove();
    if (opener && opener.isConnected) opener.focus();
  }
  function onKey(ev) {
    if (ev.key === 'Escape') close();
  }
  function onHash() {
    busy = false;
    close();
  }
  backdrop.addEventListener('click', (ev) => { if (ev.target === backdrop) close(); });

  async function run() {
    if (busy) return;
    busy = true;
    confirmBtn.disabled = true;
    cancelBtn.disabled = true;
    error.hidden = true;
    try {
      await onConfirm();
      busy = false;
      close();
    } catch (e) {
      busy = false;
      if (closed) return;
      error.textContent = e.message;
      error.hidden = false;
      confirmBtn.disabled = false;
      cancelBtn.disabled = false;
      cancelBtn.focus();
    }
  }

  document.body.append(backdrop);
  if (main) main.inert = true;
  document.addEventListener('keydown', onKey);
  window.addEventListener('hashchange', onHash);
  // The safe choice has the focus.
  cancelBtn.focus();
}
