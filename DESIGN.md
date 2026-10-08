# glyph — design

A remote-capable terminal web browser. A server drives one headless Chromium over
the DevTools Protocol (CDP), turns each tab into a character-cell grid, and streams
compact grid diffs over WebSocket. A TUI client (local or remote) draws the grid and
sends input back.

Browsh (https://github.com/browsh-org/browsh, **LGPL-2.1**) is the reference for the
*idea* only. No Browsh code is used, and none was copied or translated. Browsh is a Go
TTY client plus a Firefox web extension. The extension walks the DOM for text and sends
a downscaled canvas as per-cell colour. The Go side merges the two. glyph keeps that
two-source idea (structure + pixels) but changes the substrate:

| | Browsh | glyph |
|---|---|---|
| Engine | headless Firefox + in-page extension | Chromium over CDP, nothing injected except a ~1 KB script |
| Structure source | JS DOM walker in the extension | `DOMSnapshot.captureSnapshot` (one CDP call, layout + paint order + text boxes) |
| Pixel source | canvas draw in extension | `Page.startScreencast` JPEG frames (also the *change detector*) |
| Remote | rely on SSH/mosh to carry full ANSI frames | WebSocket, cell diffs, zstd, backpressure, token + TLS |
| Cheap mode | none | accessibility-tree text mode, resource profiles |

## Workspace

```
crates/proto   glyph-proto   cell model, grid, width/grapheme rules, messages, diff + wire codec
crates/server  glyph-server  Chromium launch, CDP client, snapshot→grid renderer, text mode,
                             profiles, tabs/sessions, WebSocket server (auth, TLS), metrics
crates/client  glyph-client  ratatui/crossterm TUI, modes, keymap, hints, omnibox, find,
                             selection/OSC 52, graphics protocols, connection (channel or WS)
crates/glyph   glyph         the binary: `local` | `serve` | `connect` | `bench-*` helpers
```

`local` wires client and server in one process through `tokio::sync::mpsc` carrying the
same `ServerMsg`/`ClientMsg` enums the WebSocket carries, so there is exactly one code
path for "the client talks to a server".

## CDP layer (own thin client, not chromiumoxide)

chromiumoxide generates thousands of typed commands (long compile times, large binary),
and we use about 25 methods. A thin client is ~300 lines: spawn Chromium with
`--remote-debugging-port=0`, read `DevToolsActivePort` from the profile dir, open the
browser WebSocket, `Target.attachToTarget{flatten:true}` per tab, and multiplex
`{id, sessionId, method, params}` / `{id, result|error}` / events. Results stay as
`serde_json::value::RawValue` until a typed consumer deserialises them, so the large
snapshot is parsed once.

One Chromium process per server. Each connected client gets its own
`Target.createBrowserContext` (isolated cookies/storage); each tab is a target in it.

## Rendering pipeline

Fixed cell box: `CW×CH = 8×16` CSS px (configurable). Viewport is
`Emulation.setDeviceMetricsOverride{width: cols*CW, height: rows*CH, deviceScaleFactor: 1}`.
`rows×cols` is the client's *content area* (terminal minus tab bar, omnibox, status bar).

Triggers (nothing is sent when idle):

1. `Page.screencastFrame` fires only when the compositor produced new pixels. It is both
   the pixel source and the "something changed" signal. The frame is acked only after the
   renderer consumed it, so Chromium itself is back-pressured.
2. On a frame (debounced, rate-capped by profile), run `DOMSnapshot.captureSnapshot`.
3. Input / navigation / resize force a refresh.

### Cell-mapping algorithm

Inputs: parsed snapshot S, optional pixmap P (decoded screencast JPEG), scroll offset.

1. **Per-node pass** (nodes are in DOM order, so `parent < child`; one linear sweep):
   `visible[i]` (visibility, effective opacity = product along ancestors),
   `clip[i]` (intersection of ancestors with `overflow != visible`; `position:fixed`
   resets, `position:absolute` takes the nearest positioned ancestor's clip),
   `interactive[i]` (nearest ancestor-or-self that is `a[href]`, button, input, textarea,
   select, `[role=button|link|…]`, or `isClickable`).
2. **Paint items** from layout nodes, in document→viewport coordinates
   (`x - scrollX`, `y - scrollY`), culled to the viewport and clip:
   - `Fill` — opaque-ish background rect (alpha ≥ 0.9 clears text below; less just tints),
   - `Border` — box-drawing outline / rules (see below),
   - `Image` — `img`, `canvas`, `video`, `svg`, `picture`, CSS `background-image` nodes,
   - `Text` — each text box = `(x, y, w, h, utf16 slice of the layout text)`.
3. **Sort by `(paintOrder, kind, layoutIndex)`** with `kind: Fill < Border < Image < Text`.
   Chromium paints, within one stacking context, all backgrounds first, then inline
   content, so this ordering is faithful, and the painter's algorithm resolves overlaps:
   later items overwrite earlier cells; an opaque `Fill` blanks text beneath it (modals,
   sticky headers, cookie banners).
4. **Geometry → cells.**
   - Column of x: `round(x/CW)`. Row of a text box: `floor((y + h/2)/CH)` (centre row, so
     a 24 px heading lands on one row, 12 px dense text on its nearest row).
   - Rect `[x0,x1)×[y0,y1)` covers cell `c` if `round(x0/CW) ≤ c < round(x1/CW)`, i.e. a
     cell is covered when ≥ 50 % of it is. Non-empty rects cover ≥ 1 cell when they are
     interactive; empty ones are dropped.
   - Text is laid on its row from its start column, one grapheme cluster at a time,
     advancing by `width(cluster)` (1 or 2, never 0). Because proportional text rarely
     equals 8 px/char, a box may spill into following cells but never past the start
     column of the next text box on the same row (it is truncated with `…` there). This
     keeps adjacent inline boxes from overwriting each other.
5. **Colour.**
   - fg: computed `color`, alpha-composited on the cell bg.
   - bg of cells with text: the *dominant* colour of the cell's pixels in P (mode over a
     4×8 subsample after 5-bit quantisation). This ignores glyph strokes, and unlike the
     CSS background it also reflects selection/find highlights and gradients. Without P
     (tests, text mode) the snapshot's blended background is used.
   - cells with an `Image` item: half-block `▀` with fg = mean of the top 8×8 pixel block,
     bg = bottom block. (Image-capable terminals additionally get the crop, below.)
   - other cells: mean pixel colour, or `▀` when the two halves differ strongly (this keeps
     1-px rules, bars and box edges visible at 8 px granularity).
   - styles: bold (weight ≥ 600), italic, underline, strikethrough from computed styles.
6. **Borders and rules.** Per node, `border-*-width/style/color`. A box with ≥ 3 rows and
   ≥ 2 cols and a visible border on all sides becomes `┌─┐│└┘` (`╭╮╰╯` when radius > 0). A
   border that is a single side, or a box thinner than 3 rows (e.g. an `<hr>`), becomes a
   `─`/`│` rule. Text items sort later, so they overwrite the rule where they collide.
7. **Interaction table.** For each interactive node: kind, `href`/label/value, backend node
   id, and the *list of line-fragment rects* (text boxes whose ancestor chain hits the
   node; a wrapped link has several; icon-only controls use their layout bounds). Each
   covered cell stores the region id in `Cell::link`. Inputs are drawn from
   `inputValue`/placeholder as `[ value___ ]` (or a box if tall enough); checkboxes
   `[x]`, radios `(•)`.
8. **Images for graphics terminals.** Image items become `ImageRegion{id, rect, hash}`;
   crops (PNG) are sent only to clients that declared a graphics protocol, and only when
   the crop's hash changed.

Known loss (documented, not hidden): content on lines shorter than 16 px collides on the
same row; text overlays inside `canvas`/`iframe` that CDP doesn't expose as text boxes are
only visible via the pixel path; hover-only UI is not triggered (no pointer-move events
are forwarded by default).

## Text mode (accessibility tree)

`Accessibility.getFullAXTree` → depth-first walk → linear blocks (heading `#…` bold, paragraph,
list items `•`, `[button]`, `[____]` inputs, `[img: alt]`, table rows joined by ` │ `, link
underlined with the AX `url` property), greedy word-wrap to `cols`, painted into a tall
virtual grid. The server slices that virtual grid by the text-mode scroll offset, so the
client stays dumb. No screencast, no screenshot, no snapshot: refresh is driven by load
events plus a debounced MutationObserver ping from the injected script. Clicking a region
resolves its `backendNodeId` to a remote object and calls `.click()`/`.focus()`.

## Protocol

`serde` + `postcard` payloads inside a 1-byte-flag frame; payloads ≥ 128 B are `zstd`
(level 3; level configurable). One WebSocket binary message = one frame. Version is checked
in the Hello exchange; mismatch is a hard error (grapheme widths come from `unicode-width`
in `glyph-proto`, so the two ends must agree on tables).

Cell run encoding mirrors terminal output: a diff is `Vec<Run{y, x, spans}>`,
`Span{style, text}`; wide-glyph continuation cells are implicit. Runs separated by ≤ 2
unchanged cells are merged (cheaper than a new run header). Unchanged cells are never sent.

```
C→S  Hello{version, token?, caps{cols,rows,cw,ch,colors,graphics,images}}
     Key Mouse Resize Navigate Back Forward Reload NewTab CloseTab SwitchTab
     Scroll Paste Find Ack{seq} SetMode{text|pixel}
S→C  Hello{version, session} FullFrame{tab,seq,…} Diff{tab,seq,base,runs}
     Regions{tab,seq,…} Title Url LoadState Tabs Cursor Clipboard Image Error FindResult
```

**Back-pressure.** The sender keeps `sent_seq`, `acked_seq`. The client acks every frame it
has *drawn*. If `sent - acked ≥ window` (default 2), the server stops emitting, and keeps
coalescing: it holds only "the latest grid", and when an ack arrives it diffs *latest vs the
client's last acked grid* (it retains that grid) and sends one diff. Intermediate frames are
therefore dropped, never queued. Target fps is adapted: ack RTT ↑ ⇒ fps ↓ (multiplicative
decrease, additive increase), bounded by the profile's cap. An idle page yields no screencast
frame, hence no CPU and no bytes.

## Security

- Bind `127.0.0.1` by default. Binding anything else requires a token; if none is configured
  one is generated and printed. Token compare is constant-time. Failed auth closes the socket
  after a fixed delay; no information is returned about why.
- Browser `Origin` headers are rejected by default (stops cross-site WebSocket hijacking of a
  localhost server by a web page the user visits).
- TLS via rustls (`--tls-cert/--tls-key`, or `--tls-self-signed` with a printed SHA-256
  fingerprint; the client pins it with `--fingerprint`).
- Not an open proxy: the only thing a client can ask for is "navigate this tab". Navigation
  schemes are allow-listed (`http`, `https`, `about:blank`; `file:`, `chrome:`, `devtools:`,
  `view-source:`, `javascript:` refused). `block_private_hosts` (default on for non-loopback
  binds) refuses literal private/loopback/link-local hosts in `Fetch` interception. This is
  best-effort: DNS names that resolve to private IPs are **not** caught (see Risks).
- Chromium's debug port is bound to loopback with a random port and an unguessable path, in a
  fresh `--user-data-dir` removed on exit.

## Resource profiles

| | lean | balanced | full |
|---|---|---|---|
| images | blocked unless client declared `images` | allowed | allowed |
| fonts / media / trackers | blocked | fonts blocked, trackers blocked | allowed |
| max fps | 4 | 10 | 20 |
| CPU throttle | 2× | 1× | 1× |
| JPEG quality | 30 | 45 | 60 |
| background tabs | discard after 60 s (URL kept) | freeze | freeze |
| site isolation | off (flag; documented trade-off) | on | on |
| animations | disabled (CSS + `prefers-reduced-motion`) | same | same |

Blocking uses `Network.setBlockedURLs` for the domain list (no round-trip per request) and
`Fetch.enable` restricted to `Image|Font|Media` resource types.

## Client

Modes: Normal (vi-style), Insert (keys go to the page), Omnibox, Find, Hint, Visual (selection).
Mouse: click → page click; wheel → scroll; drag → client-side cell selection, release copies via
OSC 52 (works over SSH). Hints: `f` labels every region from the table with home-row letters,
`F` opens in a new tab. Keymap and theme come from `~/.config/glyph/config.toml` (XDG).

## Testing strategy

- proptest: width/grapheme placement invariants (a wide head always has a continuation, no
  orphan continuation), diff/apply round-trip (`apply(a, diff(a,b)) == b`), codec round-trip, UTF-16
  slicing, rect→cell mapping monotonicity.
- insta: snapshots of rendered frames from **recorded DOMSnapshot JSON** of local HTML fixtures
  (offline, deterministic). A gated test re-records them from live Chromium via a tiny test
  HTTP server.
- Loopback WebSocket e2e: real server task + real client protocol loop, with a fake page
  backend (so CI needs no Chromium); Chromium-gated e2e when available.
- criterion: diff, codec, render.

## Risks

1. **CDP drift** (snapshot field names, screencast behaviour). Mitigation: typed structs use
   `#[serde(default)]`; live smoke tests; Chromium pinned in CI.
2. **Line density**: <16 px lines collide on one row. Mitigation: text mode; optional
   `min-line-height` CSS injection.
3. **Screencast in headless** might not fire for unchanged frames or hidden tabs — we explicitly
   request a capture on navigation, resize, tab switch.
4. **SSRF**: a remote authenticated user can make the server fetch internal URLs. Scheme allow-list
   and literal-IP blocking are partial; deployments that need more should use network egress
   policy. Stated plainly in the README.
5. **Width-table skew** between a remote client and server build — version check; spans carry
   text only, so a skew misaligns one run, not the whole frame.
6. **Throttling honesty**: CPU throttling lowers peak CPU, not total work. Benchmarks report
   both.
7. **Browsh comparison** needs Docker image `browsh/browsh` (amd64 only; emulated on Apple
   Silicon → numbers are not comparable). Reported as such, or skipped.

## Assumptions

- Chromium/Chrome ≥ 120 on PATH or `GLYPH_CHROME`; macOS and Linux first-class, Windows best-effort.
- Cell pixel box 8×16; UTF-8 terminal; truecolor assumed unless `COLORTERM` says otherwise.
- Config in `$XDG_CONFIG_HOME/glyph/config.toml` (macOS: `~/.config/glyph` too, for dotfile users).
- Project name `glyph`.
