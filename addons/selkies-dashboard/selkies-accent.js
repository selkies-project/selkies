// Sets the theme's accent in src/index.css from the logo's colors
// (src/components/logo.tsx: #D5499A, #C34AA2, #7967C5). Edit ACCENT and RULES
// below to change the brand color, then run `npm run accent`. Running it again
// with the same values changes nothing. Run it after `npx shadcn@latest init`
// rewrites the theme.
//
// The accent is shadcn's `primary` (filled buttons, switches, slider fill) and
// `ring` (focus), with the sidebar and chart tokens that follow it. `--accent`
// stays neutral: in shadcn it is the hover and selection surface, not the brand.
// Light surfaces take a deeper pink so white text clears 4.5:1; dark surfaces a
// lighter one under near-black text.
//
// RULES are the open and active states of accordions, tabs, and separators,
// written between marker comments at the end of the file so they are replaced
// in place, or added when init has removed them.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const ACCENT = {
  ':root': {
    '--primary': '#b83280',
    '--primary-foreground': '#ffffff',
    '--ring': '#d5499a',
    '--chart-1': '#d5499a',
    '--chart-2': '#7967c5',
    '--sidebar-primary': '#b83280',
    '--sidebar-primary-foreground': '#ffffff',
    '--sidebar-ring': '#d5499a',
  },
  '.dark': {
    '--primary': '#db5ba7',
    '--primary-foreground': 'oklch(0.145 0 0)',
    '--ring': '#db5ba7',
    '--chart-1': '#db5ba7',
    '--chart-2': '#9085e9',
    '--sidebar-primary': '#db5ba7',
    '--sidebar-primary-foreground': 'oklch(0.145 0 0)',
    '--sidebar-ring': '#db5ba7',
  },
};

const RULES_START = '/* selkies-accent:start */';
const RULES_END = '/* selkies-accent:end */';
const RULES = `${RULES_START}
/* The accent on the open state of accordions and tabs, for every use of them.
   Kept outside any @layer: unlayered rules beat the components' own utility
   classes (\`data-active:text-foreground\` and the like), which sit in the
   utilities layer. */
[data-slot="accordion-item"][data-open] {
  border-color: color-mix(in oklab, var(--primary) 50%, transparent);
}

[data-slot="accordion-trigger"][aria-expanded="true"],
[data-slot="tabs-trigger"][data-active] {
  color: var(--primary);
}

[data-slot="tabs-trigger"][data-active] {
  border-color: color-mix(in oklab, var(--primary) 40%, transparent);
}

/* Separators carry a hint of the accent: the kit's separator, the menus' and
   button groups', and the rule under an open accordion header. */
[data-slot="separator"],
[data-slot="button-group-separator"],
[data-slot="menu-separator"],
[data-slot="dropdown-menu-separator"] {
  background-color: color-mix(in oklab, var(--primary) 35%, var(--border));
}

[data-slot="accordion-content"] > div {
  border-top-color: color-mix(in oklab, var(--primary) 35%, var(--border));
}
${RULES_END}`;

const here = path.dirname(fileURLToPath(import.meta.url));
const file = path.resolve(here, 'src/index.css');
const raw = fs.readFileSync(file, 'utf8');
const crlf = raw.includes('\r\n');
let css = raw.replace(/\r\n/g, '\n');

let failed = false;
for (const [selector, tokens] of Object.entries(ACCENT)) {
  const open = css.indexOf(`\n${selector} {`);
  const close = open < 0 ? -1 : css.indexOf('\n}', open);
  if (open < 0 || close < 0) {
    console.error(`ERROR: no ${selector} block in src/index.css`);
    failed = true;
    continue;
  }
  let block = css.slice(open, close);
  for (const [name, value] of Object.entries(tokens)) {
    const line = new RegExp(`^(\\s*${name}:\\s*)[^;]+;`, 'm');
    if (!line.test(block)) {
      console.error(`ERROR: ${name} is not set in ${selector}`);
      failed = true;
      continue;
    }
    block = block.replace(line, (_, head) => `${head}${value};`);
  }
  css = css.slice(0, open) + block + css.slice(close);
}
if (failed) process.exit(1);

const startAt = css.indexOf(RULES_START);
const endAt = css.indexOf(RULES_END);
if (startAt >= 0 && endAt > startAt) {
  css = css.slice(0, startAt) + RULES + css.slice(endAt + RULES_END.length);
} else {
  css = `${css.replace(/\s+$/, '')}\n\n${RULES}\n`;
}

fs.writeFileSync(file, crlf ? css.replace(/\n/g, '\r\n') : css, 'utf8');
console.log('accent: src/index.css updated');
