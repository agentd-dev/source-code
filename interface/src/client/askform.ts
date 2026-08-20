// SPDX-License-Identifier: AGPL-3.0-only
/**
 * Reading a gate's answer schema into a form both clients can render.
 *
 * A question and a text box makes the person guess the wording the schema will
 * accept — and when they guess wrong the answer is rejected and they guess
 * again. The schema already says what the acceptable answers ARE, so the client
 * should offer them.
 *
 * Deliberately a small subset of JSON Schema: the shapes a person can be asked
 * for in one interaction. Anything else falls back to free text, which is what
 * happened to every gate before this existed, so the fallback is not a
 * regression.
 */
import type { Json } from './types.js';

export type AskForm =
  /** Pick exactly one. */
  | { kind: 'one'; options: string[]; other: boolean; def?: string }
  /** Pick any number. */
  | { kind: 'many'; options: string[]; other: boolean; def?: string[] }
  /** Yes or no. */
  | { kind: 'bool'; def?: boolean }
  /** Anything else — a text box, as before. */
  | { kind: 'text'; def?: string };

function enumOf(v: Json | undefined): string[] | null {
  if (!v || typeof v !== 'object' || Array.isArray(v)) return null;
  const o = v as { [k: string]: Json };
  const e = o.enum;
  if (Array.isArray(e) && e.length > 0 && e.every((x) => typeof x === 'string')) {
    return e as string[];
  }
  return null;
}

/**
 * `other` is offered when the schema says a value OUTSIDE the list is
 * acceptable — an `anyOf` pairing the enum with a plain string. Without that
 * signal, offering a free-text box would invite an answer the schema then
 * rejects, which is worse than not offering it.
 */
function withOther(v: { [k: string]: Json }): { options: string[]; other: boolean } | null {
  const direct = enumOf(v);
  if (direct) return { options: direct, other: false };
  const branches = (v.anyOf ?? v.oneOf) as Json[] | undefined;
  if (!Array.isArray(branches)) return null;
  let options: string[] | null = null;
  let other = false;
  for (const b of branches) {
    const e = enumOf(b);
    if (e) {
      options = [...(options ?? []), ...e];
      continue;
    }
    const bo = b as { [k: string]: Json } | null;
    if (bo && typeof bo === 'object' && bo.type === 'string') other = true;
  }
  return options ? { options, other } : null;
}

export function askForm(schema: Json | undefined): AskForm {
  if (!schema || typeof schema !== 'object' || Array.isArray(schema)) return { kind: 'text' };
  const s = schema as { [k: string]: Json };

  // A single-property object is the common gate shape (`{decision: …}`);
  // unwrap it so the person is asked the question, not shown a JSON envelope.
  if (s.type === 'object') {
    const props = s.properties as { [k: string]: Json } | undefined;
    const keys = props ? Object.keys(props) : [];
    if (keys.length === 1) return askForm(props![keys[0]]);
    return { kind: 'text' };
  }

  if (s.type === 'boolean') {
    return { kind: 'bool', def: typeof s.default === 'boolean' ? s.default : undefined };
  }

  if (s.type === 'array') {
    const items = s.items as { [k: string]: Json } | undefined;
    const picked = items ? withOther(items) : null;
    if (picked) {
      const def = Array.isArray(s.default) ? (s.default as Json[]).filter((d): d is string => typeof d === 'string') : undefined;
      return { kind: 'many', options: picked.options, other: picked.other, def };
    }
    return { kind: 'text' };
  }

  const picked = withOther(s);
  if (picked) {
    return {
      kind: 'one',
      options: picked.options,
      other: picked.other,
      def: typeof s.default === 'string' ? s.default : undefined,
    };
  }
  return { kind: 'text', def: typeof s.default === 'string' ? s.default : undefined };
}

/** The answer a form produces, in the shape the schema asked for. */
export function askAnswer(form: AskForm, picked: string[], other: string): Json {
  switch (form.kind) {
    case 'many':
      return [...picked.filter((p) => p !== '__other__'), ...(other.trim() ? [other.trim()] : [])];
    case 'bool':
      return picked[0] === 'yes';
    case 'one':
      return picked[0] === '__other__' ? other.trim() : (picked[0] ?? '');
    default:
      return other.trim();
  }
}
