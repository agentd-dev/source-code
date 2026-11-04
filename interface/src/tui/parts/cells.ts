// SPDX-License-Identifier: AGPL-3.0-only
/**
 * Text measured in terminal columns, not code points: an East Asian wide or
 * fullwidth character and an emoji take two columns, a combining mark or a
 * zero-width joiner none. A small table, not a full Unicode width database —
 * enough that a row cut "to fit" does not wrap on the characters people
 * actually type.
 */

/** Ranges of code points two columns wide (inclusive). */
const WIDE: ReadonlyArray<readonly [number, number]> = [
  [0x1100, 0x115f],
  [0x231a, 0x231b],
  [0x2329, 0x232a],
  [0x23e9, 0x23ec],
  [0x23f0, 0x23f0],
  [0x23f3, 0x23f3],
  [0x25fd, 0x25fe],
  [0x2614, 0x2615],
  [0x2648, 0x2653],
  [0x267f, 0x267f],
  [0x2693, 0x2693],
  [0x26a1, 0x26a1],
  [0x26aa, 0x26ab],
  [0x26bd, 0x26be],
  [0x26c4, 0x26c5],
  [0x26ce, 0x26ce],
  [0x26d4, 0x26d4],
  [0x26ea, 0x26ea],
  [0x26f2, 0x26f3],
  [0x26f5, 0x26f5],
  [0x26fa, 0x26fa],
  [0x26fd, 0x26fd],
  [0x2705, 0x2705],
  [0x270a, 0x270b],
  [0x2728, 0x2728],
  [0x274c, 0x274c],
  [0x274e, 0x274e],
  [0x2753, 0x2755],
  [0x2757, 0x2757],
  [0x2795, 0x2797],
  [0x27b0, 0x27b0],
  [0x27bf, 0x27bf],
  [0x2b1b, 0x2b1c],
  [0x2b50, 0x2b50],
  [0x2b55, 0x2b55],
  [0x2e80, 0x303e],
  [0x3041, 0x33ff],
  [0x3400, 0x4dbf],
  [0x4e00, 0x9fff],
  [0xa000, 0xa4cf],
  [0xa960, 0xa97f],
  [0xac00, 0xd7a3],
  [0xf900, 0xfaff],
  [0xfe10, 0xfe19],
  [0xfe30, 0xfe6f],
  [0xff00, 0xff60],
  [0xffe0, 0xffe6],
  [0x16fe0, 0x16fe4],
  [0x17000, 0x18aff],
  [0x1b000, 0x1b2ff],
  [0x1f004, 0x1f004],
  [0x1f0cf, 0x1f0cf],
  [0x1f18e, 0x1f18e],
  [0x1f191, 0x1f19a],
  [0x1f200, 0x1f251],
  [0x1f300, 0x1f64f],
  [0x1f680, 0x1f6ff],
  [0x1f7e0, 0x1f7eb],
  [0x1f900, 0x1f9ff],
  [0x1fa70, 0x1faff],
  [0x20000, 0x3fffd],
];

/** Ranges of code points that take no column of their own. */
const ZERO: ReadonlyArray<readonly [number, number]> = [
  [0x0300, 0x036f],
  [0x1ab0, 0x1aff],
  [0x1dc0, 0x1dff],
  [0x200b, 0x200f],
  [0x20d0, 0x20ff],
  [0xfe00, 0xfe0f],
  [0xfe20, 0xfe2f],
  [0xe0100, 0xe01ef],
];

function inRanges(cp: number, ranges: ReadonlyArray<readonly [number, number]>): boolean {
  let lo = 0;
  let hi = ranges.length - 1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    const [a, b] = ranges[mid];
    if (cp < a) hi = mid - 1;
    else if (cp > b) lo = mid + 1;
    else return true;
  }
  return false;
}

/** The columns one character (a code point, as `[...s]` yields them) takes. */
export function charColumns(ch: string): number {
  const cp = ch.codePointAt(0) ?? 0;
  if (cp < 0x20 || (cp >= 0x7f && cp < 0xa0)) return 0;
  if (inRanges(cp, ZERO)) return 0;
  return inRanges(cp, WIDE) ? 2 : 1;
}

/** The columns `s` takes on one row. */
export function columnsOf(s: string): number {
  let n = 0;
  for (const ch of s) n += charColumns(ch);
  return n;
}

/** The longest start of `s` that fits in `cols` columns. */
export function cutToColumns(s: string, cols: number): string {
  let used = 0;
  let out = '';
  for (const ch of s) {
    const w = charColumns(ch);
    if (used + w > cols) break;
    used += w;
    out += ch;
  }
  return out;
}

/**
 * `s` (one line, no newline) broken into rows: the first `first` columns
 * wide, every other `width` wide. Broken after the last space that fits, or
 * at the column when a word is longer than the row — and every row fits, so
 * the rows a caller counts are the rows it draws.
 */
export function breakRows(s: string, first: number, width: number): string[] {
  const rows: string[] = [];
  let row: string[] = [];
  let used = 0;
  let room = Math.max(1, first);
  for (const ch of s) {
    const w = charColumns(ch);
    if (used + w > room && row.length > 0) {
      const space = row.lastIndexOf(' ');
      if (ch === ' ') {
        rows.push(row.join(''));
        row = [];
      } else if (space > 0) {
        rows.push(row.slice(0, space).join(''));
        row = row.slice(space + 1);
      } else {
        rows.push(row.join(''));
        row = [];
      }
      used = columnsOf(row.join(''));
      room = Math.max(1, width);
      if (ch === ' ') continue;
    }
    row.push(ch);
    used += w;
  }
  rows.push(row.join(''));
  return rows;
}
