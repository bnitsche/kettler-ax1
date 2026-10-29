// Rider profile form: parse raw input strings into a PUT body (same limits and texts as the server).

const WEIGHT_ERR = 'Gewicht muss zwischen 30 und 250 kg liegen';
const FLAT_ERR = 'Ebene muss zwischen 0 und 300 W liegen';

/**
 * Parse the form strings against `person`; `body` holds only the changed fields. `strap` is the chosen
 * strap id as text ('' = none); it is left out when the form has no strap choice.
 */
export function parseProfile({ name, weight, flat, strap }, person) {
  const body = {};
  const trimmed = String(name).trim();
  if (!trimmed) return { ok: false, error: 'Name darf nicht leer sein' };
  if (trimmed !== person.name) body.name = trimmed;

  const w = String(weight).trim().replace(',', '.');
  const weightKg = /^\d+(\.\d+)?$/.test(w) ? Number(w) : NaN;
  if (!(weightKg >= 30 && weightKg <= 250)) return { ok: false, error: WEIGHT_ERR };
  if (weightKg !== person.weight_kg) body.weight_kg = weightKg;

  const f = String(flat).trim();
  const flatWatts = /^\d+$/.test(f) ? Number(f) : NaN;
  if (!(flatWatts >= 0 && flatWatts <= 300)) return { ok: false, error: FLAT_ERR };
  if (flatWatts !== person.flat_watts) body.flat_watts = flatWatts;

  if (strap !== undefined) {
    const strapId = strap === '' ? null : Number(strap);
    if (strapId !== (person.strap_id ?? null)) body.strap_id = strapId;
  }

  if (Object.keys(body).length === 0) return { ok: false, error: 'Keine Änderung' };
  return { ok: true, body };
}
