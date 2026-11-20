# Kern brand

## Mark

The mark is a geometric **K** — the letter the name comes from (kerning: tightening the space
between letters) and a nod to *kernel*: the core the runtime is built around. It is drawn on a
32-unit grid, 4.75-unit strokes, flat ends, one radius on the stem.

| File | Use |
|------|-----|
| `kern-mark.svg` | Glyph only, `currentColor` — the in-app mark on any surface |
| `kern-icon.svg` | Amber tile, ink glyph — app icon, Windows `.ico`, macOS `.icns` |
| `kern-icon-dark.svg` | Ink tile, amber glyph — dark docks, favicons on light pages |

Clear space: at least the stem width (4.75 units) on all sides. Never redraw the arms as a
separate chevron: the gap between stem and arms is a kerning accident, not a design feature.

## Colour

One accent, one job: **amber is the build hours burning**. Everything else is ink and neutral.

| Token | Hex | Role |
|-------|-----|------|
| `--ink-950` | `#08090B` | App background |
| `--ink-900` | `#0E1013` | Rails, panels |
| `--ink-850` | `#15181D` | Raised surfaces, inputs |
| `--line` | `#22262E` | Hairlines, borders |
| `--text` | `#E8EBF0` | Body text |
| `--muted` | `#8A929F` | Secondary text, labels |
| `--amber` | `#FFB020` | Accent: hours, active agents, focus |
| `--amber-press` | `#E89A12` | Pressed / hover accent |
| `--red` | `#FF5D5D` | Failure only |

Amber is never used for decoration. If something is amber, it is live or it is time.

## Type

- UI: `Inter`, falling back to `system-ui`. Headings tight (`-0.02em`), labels uppercase with
  `0.12em` tracking.
- Code, numbers of record, and the hours counter: `JetBrains Mono`, tabular figures.
- The wordmark is uppercase `KERN` at 700 weight, `-0.03em` tracking — the one place the name is
  kerned tight, on purpose.

## Voice

Short, technical, no exclamation marks. Say the number. "4h 12m left", not "You're doing great!".
The app is a tool for people who already know what a token budget is.
