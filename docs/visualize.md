# Visualizing a graph

```sh
chitra visualize --out graph.html
```

Then open the file. Nothing is fetched at view time — no CDN, no build step, no
server. It works on a plane.

```
wrote graph.html (682 KB) — open it in a browser; nothing is fetched at view time
```

## What you are looking at

The map is drawn as a **survey plate**. Modules are sectors, symbols are points
within them, and **how much depends on a symbol is its brightness** — so the
things everything else leans on are the things that catch your eye first.

Layout is computed in Rust and baked into the file, so the browser only draws.
Geometry ships as base64 typed arrays and renders on WebGL2 — one draw call for
every symbol, one for every link — with a canvas-2D fallback.

## Reading it

| | |
|---|---|
| **Labels** | The most depended-upon symbols stay labelled at every zoom level, so you always know where you are. |
| **Link direction** | Links fade from caller to callee, so direction is visible without arrowheads at any density. |
| **Blast radius** | Select a symbol and its dependents expand outward hop by hop. |
| **Call path** | Shift-click a second symbol to trace the shortest call path between the two. |
| **Module focus** | Click a sector to isolate it and zoom in. The breadcrumb takes you back. |
| **Search** | `/` focuses the search box, `Enter` cycles matches, `Esc` steps back out. |

Ambiguous edges are never drawn, for the same reason they never drive impact
analysis: chitra will not show you a relationship it cannot defend.

All motion respects `prefers-reduced-motion`.

## Modes and limits

```sh
chitra visualize --mode community      # default: modules as regions
chitra visualize --mode full           # every symbol
chitra visualize --max-nodes 50000     # default 20,000
```

The default view is deliberately not the whole graph. At tens of thousands of
edges the limit is legibility, not frame rate — a hairball renders fine and
tells you nothing.

## Performance

| Graph | Page | Time to generate |
|---|---|---|
| 5,478 symbols | 682 KB | 0.14 s |
| 16,585 symbols | 2.2 MB | 0.21 s |

Output is byte-stable, like every other artefact chitra produces.
