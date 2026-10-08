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
| Remote | rely on SSH/mosh to carry full ANSI frames | WebSocket, cell diffs, streaming zstd, back-pressure, token + TLS |
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
client stays dumb. No screencast and no pixels. Each rebuild is one AX-tree call plus a `display`-only
DOMSnapshot (block vs inline); in between, scrolling re-slices the cached document with no CDP
traffic at all. Rebuilds are driven by load events plus a debounced MutationObserver / `input` ping
from the injected script. Clicking a region
resolves its `backendNodeId` to a remote object and calls `.click()`/`.focus()`.

## Protocol

`serde` + `postcard` payloads. One WebSocket binary message = `[flag][payload]`; flag 0 is raw
postcard, flag 1 is a chunk of a **streaming zstd context** that lives as long as the connection
(level 3 by default, 128 KiB window). Messages under 96 B are sent raw and are *not* fed to the
compressor, so both contexts stay in step. Streaming matters: one diff is a few hundred bytes (no
ratio alone) but consecutive diffs share colours, run headers and text, which the shared window
exploits. The decoder caps output *while* expanding (32 MiB), so a small hostile message cannot
inflate into memory. Version is checked in the Hello exchange; mismatch is a hard error (grapheme
widths come from `unicode-width` in `glyph-proto`, so the two ends must agree on tables).

Cell run encoding mirrors terminal output: a diff is `Vec<Run{y, x, spans}>`,
`Span{style, text}`; wide-glyph continuation cells are implicit. Runs separated by ≤ 2
unchanged cells are merged (cheaper than a new run header). Unchanged cells are never sent. A span
is split wherever re-segmenting its concatenated text would not give back the cell boundaries
(regional-indicator pairs, combining marks across cells) — found by a property test.

```
C→S  Hello{version, token?, caps{cols,rows,cell_px,graphics,scheme,images}}
     Key Mouse Resize Navigate Back Forward Reload Stop NewTab CloseTab SwitchTab
     Scroll Paste Find Ack{tab,seq} SetMode{text|pixel} ClearFocus Redraw
S→C  Hello{version, session, profile} FullFrame{tab,seq,…} Diff{tab,seq,base,runs}
     Regions{tab,seq,…} Title Url LoadState Tabs Cursor Clipboard Image ImageClear
     FindResult Scroll Mode Error
```

**Back-pressure.** The transport is ordered and reliable, so "what the client has" is simply the
last frame we sent. The outbox keeps `seq` and `acked`. The client acks a frame only **after it has
been written to the terminal**, so a slow terminal slows the server down. While `seq - acked ≥
window` (default 2) nothing is sent and only the *latest* offered grid is kept; when an ack frees a
slot, one diff `last-sent → latest` goes out. Intermediate states are dropped, never queued
(verified over a real socket, and mutation-checked: removing the window check makes the test fail).
Sequence numbers are monotonic across tab switches so a stale ack can never acknowledge a newer
frame. The frame rate adapts to ack round-trip time (multiplicative decrease above 250 ms, additive
increase below 100 ms) and is capped by the profile. Chromium itself is throttled the same way: its
screencast frame is acked only after we rendered it (and **every** frame must be acked exactly once,
or Chromium stalls after two). An idle page yields no frames, hence no CPU and no bytes.

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
| CPU throttle | off (measured harmful) | off | off |
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

## Risks (status after the build)

1. **CDP drift** (snapshot field names, screencast behaviour). Typed structs use `#[serde(default)]`;
   live tests run against real Chromium. Verified on Chrome 154 only.
2. **Line density**: <16 px lines collide on one row; proportional text is ~10 % longer in cells
   than in pixels. Mitigations in the renderer (chaining adjacent inline boxes, never overrunning the
   next box's start); reader mode is the real answer. Still visible on some pages.
3. **Screencast in headless**: frames can be *older than the DOM snapshot* (observed right after a
   scroll, drawing the previous page into empty cells). The frame carries its scroll offset; if it
   disagrees with the snapshot's we take a fresh screenshot. Smooth scrolling is disabled so there
   are no intermediate frames.
4. **SSRF**: a remote client makes the server fetch URLs. Done: scheme allow-list, private/loopback/
   link-local **host literals** refused for documents/XHR/fetch on remote servers. Not done: a DNS
   name that resolves to a private address is not caught (that needs resolver-level control).
   Deployments that need more must restrict egress. README says so.
5. **Width-table skew** between client and server builds: version check; a skew misaligns one run,
   not the frame. Terminals also disagree with `unicode-width` on some emoji sequences.
6. **CPU throttling is counter-productive** (measured): `setCPUThrottlingRate` 2× → an idle page uses
   57 % of a core, 4× → 84 %, versus ~1 % unthrottled. Disabled in all profiles; kept as an opt-in.
7. **Browsh comparison**: not run. The Docker daemon was not running on the build machine, the
   image is amd64-only (emulated on Apple Silicon, so numbers would not be comparable), and a native
   Browsh needs Firefox. Reported as skipped rather than invented.
8. **Graphics protocols** could only be checked at the byte level (no Kitty/iTerm2/Sixel terminal
   available to the build); they are the least-verified part of the product.

## Things the implementation taught us (deviations from the first design)

* **Fetch interception needs its own task.** `Page.navigate` does not return until its Document
  request is resolved; resolving it is done by handling `Fetch.requestPaused`; if the same loop
  awaits both, it deadlocks. (Found only by running a real session against a blocked URL.)
* **Memory numbers**: summing per-process RSS over Chromium's ~10 processes overstates real use
  2-5x. The harness reports macOS `footprint` / Linux PSS and also the naive sum, and counts glyph's
  own process separately (its children must not be counted twice).
* **`--disable-features` is last-one-wins** in Chromium; the launcher merges all sources into one
  switch.
* **Background colour of text cells**: the "dominant colour" of a cell is wrong when a large glyph
  out-votes its own background (white-on-dark, 32 px bold). The sample ignores pixels near the known
  text colour, and neighbouring near-equal colours are snapped together (JPEG noise otherwise breaks
  every span and wastes bandwidth).
* **Reader mode needs two sources**: the AX tree has the semantics but not block-vs-inline, so a
  `display`-only DOMSnapshot is joined on `backendNodeId`.
* **Lifetime**: `kill_on_drop` does nothing when the parent is SIGKILLed; a detached `sh` watchdog
  (parent-liveness poll) kills Chromium and removes the profile directory.

## Assumptions

- Chromium/Chrome ≥ 120 on PATH or `GLYPH_CHROME`; macOS and Linux first-class, Windows best-effort
  (the watchdog is Unix-only).
- Cell pixel box 8×16; UTF-8 terminal; truecolor assumed unless `COLORTERM`/`TERM` say otherwise.
- Config in `$XDG_CONFIG_HOME/glyph/config.toml` (`~/.config/glyph` otherwise).
- Project name `glyph`; crates `glyph-proto`, `glyph-server`, `glyph-client`, binary `glyph`.
