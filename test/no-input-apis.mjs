// Fails if ghost's source calls any API that moves the real cursor or injects global input.
import { readFileSync, readdirSync, existsSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const FORBIDDEN = /\b(SetCursorPos|SendInput|mouse_event|keybd_event|SetPhysicalCursorPos|ClipCursor|BlockInput|InjectSyntheticPointerInput|InjectTouchInput|InjectMouseInput|InjectKeyboardInput|InjectPenInput|CreateSyntheticPointerDevice)\b/g;
const find = (text) => [...text.matchAll(FORBIDDEN)].map((m) => m[1]);

// Positive control: the matcher must catch a planted call or the absence check means nothing.
if (find('unsafe { SendInput(&inputs, size) };').length !== 1) { console.log('CONTROL FAILED'); process.exit(1); }

const root = fileURLToPath(new URL('..', import.meta.url));
const files = [];
const walk = (d) => { for (const e of readdirSync(d, { withFileTypes: true })) { const p = join(d, e.name); if (e.isDirectory()) walk(p); else if (/\.(rs|cs|go)$/.test(e.name)) files.push(p); } };
if (existsSync(join(root, 'src'))) walk(join(root, 'src'));
if (existsSync(join(root, 'ghost.cs'))) files.push(join(root, 'ghost.cs'));
if (!files.length) { console.log('NO SOURCE FOUND'); process.exit(1); }

const hits = files.flatMap((f) => find(readFileSync(f, 'utf8')).map((h) => `${h} in ${f}`));
if (hits.length) { console.log('FORBIDDEN APIs: ' + [...new Set(hits)].join(', ')); process.exit(1); }
console.log(`no global input APIs (${files.length} files)`);
