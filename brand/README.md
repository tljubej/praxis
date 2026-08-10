# The mark

Π, the initial of πρᾶξις, drawn as a gate: one post is still an outline, the
other is built and raked. Theory on the left, the deed on the right, and a beam
that only stands because of both.

```
praxis-mark.svg          two-tone, self-theming   -> README, releases, anywhere it travels alone
praxis-mark-inline.svg   currentColor + --mark    -> inlined into a page that has its own theme
praxis-mark-mono.svg     one colour               -> stencils, one-colour print, embroidery
```

Nothing loads these at runtime, because both consumers have to be self-contained.
`www/index.html` is a single file that makes no network requests, so the header
mark is inlined into the markup and the favicon is inlined as a `data:` URI; a
`.vsix` ships its own assets, so the extension carries its own copies too.

**The geometry therefore exists in four places, and no build step reconciles
them.** Change one and change the rest by hand:

| where | what |
| ----- | ---- |
| `brand/*.svg` | the source of truth |
| `www/index.html` | inline SVG in the header, plus the favicon `data:` URI |
| `editors/vscode/icon.png` | 128×128 marketplace tile, rasterised |
| `editors/vscode/icons/praxis-file-*.svg` | the `.px` file icon, one per UI theme |

The marketplace tile must be a **PNG** — VS Code does not accept SVG for
`package.json`'s `icon` — and it is drawn on the solid `#0F0C0D` ground rather
than on transparency, so it survives both the light marketplace page and the
dark extensions sidebar. To regenerate it, render `praxis-mark.svg` centred at
96px inside a 128px `#0F0C0D` tile.

## Construction

Canvas 128×128, cell 16, no corner radius anywhere. Two rules hold the mark
together, and breaking either is what makes the diagonal look stuck on:

1. **Every member is 19 units wide.**
2. **Every member ends on a joint.**

| member | geometry |
| ------ | -------- |
| lintel | `x 14→120, y 14→33` — overhangs 8 left, 14 right |
| outlined post | outer `x 22→41, y 33→116`, 7-unit stroke, open at the top |
| raked strut | `87,33 106,33 120,116 101,116` — rake 14 over 83, just under 10° |

The rake and the right overhang are both 14 units, so the strut's foot lands at
`x 120`, exactly where the lintel ends. Change one without the other and the
overhang stops being explained by anything.

## Two traps, both already paid for

**Keep `fill` and `stroke` on separate classes.** A CSS rule outranks a
presentation attribute, so a class that sets `fill` overrides the outlined post's
`fill="none"` and closes its counter — the post goes solid and the mark loses the
one contrast it is built on. `praxis-mark.svg` splits `.solid` from `.open` for
exactly this reason.

**An SVG behind `<img>` follows the operating system, not the page.**
`prefers-color-scheme` inside an `<img>`-referenced or `href`-referenced SVG
resolves against the OS. That is correct for a favicon — browser chrome follows
the OS too — and wrong for a page with its own theme toggle, where the mark would
follow the OS while the page follows the button. Use `praxis-mark-inline.svg`
there, inlined into the DOM so `currentColor` and `var(--mark)` resolve against
the document.

## Colour

The frame takes the surrounding ink. Only the strut is `--mark`.

| token | light | dark |
| ----- | ----- | ---- |
| `--ink` | `#171214` | `#EAE3E4` |
| `--mark` | `#C8102E` | `#F04555` |

`--mark` is deliberately not one of the page's three semantic hues. `signal`,
`type` and `fault` each mean something the compiler means by them; the strut
means the language, so it is the one colour on the page allowed to mean nothing.
The site aliases it as `--accent` and spends it on chrome — what you can press,
and what counts as ours.
It is also close to `--fault` (`#BE3418` / `#FF6B4A`) and must never be used for
anything a diagnostic could use it for.

## Clear space and minimum size

Clear space on all four sides is one member width — 19 units, 15% of the mark's
height. Drawn to hold at 16px. The outlined post's counter closes below about
24px, which is expected: it degrades to a solid Π rather than to a smudge.
