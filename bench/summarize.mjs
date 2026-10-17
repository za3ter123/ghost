// Prints the tables in docs/benchmarks.md from the benchmark rows. Node, no dependencies.
//   node bench/summarize.mjs              read bench/results.public.jsonl, print markdown
//   node bench/summarize.mjs --export     write results.public.jsonl from the raw results.jsonl (drops `result`)
import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';

const here = dirname(fileURLToPath(import.meta.url));
const load = (f) => readFileSync(join(here, f), 'utf8').split(/\r?\n/)
  .map((s) => s.replace(/^﻿/, '').trim()).filter(Boolean).map((s) => JSON.parse(s));

if (process.argv.includes('--export')) {
  // The free-text `result` can hold local paths, so the public copy leaves it out.
  const rows = load('results.jsonl').map(({ result, ...r }) => r);
  writeFileSync(join(here, 'results.public.jsonl'), rows.map((r) => JSON.stringify(r)).join('\n') + '\n');
  console.log(`wrote ${rows.length} rows to results.public.jsonl`);
  process.exit(0);
}

const median = (xs) => {
  const s = xs.filter((x) => typeof x === 'number').sort((a, b) => a - b);
  if (!s.length) return null;
  const m = s.length >> 1;
  return s.length % 2 ? s[m] : (s[m - 1] + s[m]) / 2;
};
assert.equal(median([3, 1, 2]), 2);
assert.equal(median([4, 1, 3, 2]), 2.5);
assert.equal(median([null, undefined]), null);

const rows = load('results.public.jsonl');
const CURRENT = ['gate-20261017-152449', 'gate-20261017-150207', 'chrome-w2'];
const TASKS = ['W1', 'W2', 'A1', 'N1', 'N2'];
const TOOLS = ['ghost', 'agent-browser', 'playwright', 'chrome', 'none'];
const NAME = { ghost: 'ghost', 'agent-browser': 'agent-browser', playwright: 'Playwright MCP', chrome: 'Claude in Chrome', none: 'no tool' };
const current = rows.filter((r) => CURRENT.includes(r.tag));
const before = rows.filter((r) => !r.tag);
// Ran on the build before bare `read` and the always-run final `close`, so it measures an older ghost.
const OLDER_GATE = 'gate-20261017-144352';

const k = (n) => (n == null ? 'n/a' : `${(n / 1000).toFixed(1)}k`);
const int = (n) => (n == null ? 'n/a' : String(Math.round(n)));
const sec = (n) => (n == null ? 'n/a' : `${(n / 1000).toFixed(1)} s`);
const stats = (rs) => ({
  ok: `${rs.filter((r) => r.ok).length}/${rs.length}`,
  total: median(rs.map((r) => r.total)), turns: median(rs.map((r) => r.turns)),
  wall: median(rs.map((r) => r.wall_ms)), tool: median(rs.map((r) => r.tool_tokens)),
});

function detail(title, rs) {
  console.log(`### ${title}\n`);
  console.log('| Task | Tool | Correct | Median total tokens | Median turns | Median wall time | Median tool tokens |');
  console.log('|---|---|---|---|---|---|---|');
  for (const t of TASKS) for (const tool of TOOLS) {
    const g = rs.filter((r) => r.task === t && r.tool === tool);
    if (!g.length) continue;
    const s = stats(g);
    console.log(`| ${t} | ${NAME[tool]} | ${s.ok} | ${int(s.total)} | ${int(s.turns)} | ${sec(s.wall)} | ${int(s.tool)} |`);
  }
  console.log('');
}

// Headline: median total tokens and correct/runs. Playwright MCP has no current rows, so its cells come from `before`.
console.log('### Headline\n');
console.log('| Task | ghost | agent-browser | Playwright MCP (earlier round) | Claude in Chrome | no tool |');
console.log('|---|---|---|---|---|---|');
for (const t of TASKS) {
  const cell = (src, tool) => {
    const g = src.filter((r) => r.task === t && r.tool === tool);
    if (!g.length) return '';
    const s = stats(g);
    return `${k(s.total)} (${s.ok})`;
  };
  console.log(`| ${t} | ${cell(current, 'ghost')} | ${cell(current, 'agent-browser')} | ${cell(before, 'playwright')} | ${cell(current, 'chrome')} | ${cell(current, 'none')} |`);
}
console.log('');

detail('Current round, both gate runs and the Claude in Chrome run together', current);
for (const tag of CURRENT.slice(0, 2)) detail(`Gate run ${tag} on its own`, rows.filter((r) => r.tag === tag));
detail('Before this round (untagged rows, earlier ghost versions)', before);
detail(`Gate run ${OLDER_GATE} (the build before bare read and close-always)`, rows.filter((r) => r.tag === OLDER_GATE));

const per = rows.filter((r) => r.turns > 0 && r.total > 0).map((r) => r.total / r.turns);
console.log('### Context per turn, all rows\n');
console.log(`Rows: ${per.length}. Total tokens divided by turns: lowest ${int(Math.min(...per))}, median ${int(median(per))}.`);
const skipped = [...new Set(rows.filter((r) => r.tag && r.tag !== OLDER_GATE && !CURRENT.includes(r.tag)).map((r) => r.tag))];
if (skipped.length) console.log(`\nTagged rows not shown above: ${skipped.join(', ')}.`);
