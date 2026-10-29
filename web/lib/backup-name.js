// Texts for backup and restore on the service page.

const pad = (n) => String(n).padStart(2, '0');

/** File name of a downloaded backup, in local time: kettler-backup-2026-10-27-0905.db */
export function backupName(date) {
  const day = `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}`;
  return `kettler-backup-${day}-${pad(date.getHours())}${pad(date.getMinutes())}.db`;
}

const count = (n, one, many) => `${n} ${n === 1 ? one : many}`;

/** What a backup holds, e.g. "2 Fahrer, 7 Programme und 1 Fahrt". */
export function restoreText({ persons, programs, rides }) {
  return `${count(persons, 'Fahrer', 'Fahrer')}, ${count(programs, 'Programm', 'Programme')} und ${count(rides, 'Fahrt', 'Fahrten')}`;
}
