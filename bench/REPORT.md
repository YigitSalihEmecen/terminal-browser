# glyph benchmark report

All numbers on this page were produced by `glyph bench` (and `cargo bench`) on one machine; the raw
rows are in `bench/results/`. They are **one machine, one browser, synthetic local pages**: read them
as "what this design costs", not as a general claim about the web.

| | |
|---|---|
| machine | Apple M4 (10 cores), 16 GB, macOS 26.5 |
| browser | Google Chrome 154.0.8037.98 (headless=new), one process tree per server |
| glyph | working tree after milestone M5 plus the M6 changes described below |
| client | the real client protocol stack over a loopback WebSocket (postcard + streaming zstd, acks like the TUI), 120×40 cells, no TUI drawing |
| pages | `bench/pages/`: **static** (28 KB article, 30 sections), **dense** (71 KB, 300-row × 6-column table, 900 links), **ticker** (a counter changing 5×/s + a CSS spinner), **images** (six scaled images) |
| scenarios | **idle** (nobody touches the page), **scroll** (up/down over the same ~24 lines every 0.4 s), **read** (steady scroll through *fresh* content) |
| window | 20 s measured after a 3 s warm-up; a fresh Chromium per profile × page |

## What the columns mean

* **Chromium mem MB** – memory of Chromium's whole process tree, *shared-aware* (macOS `footprint`'s
  summary line; PSS on Linux). **(rss sum)** is the naive sum of per-process resident sizes. It is shown
  because it is what `ps`/`top` users will compare against: it counts shared pages once per process and is
  ~3.2× larger here.
* **glyph MB** – glyph's own process (server logic plus this harness's trivial client), not Chromium.
* **CPU %** – CPU time used by Chromium's tree plus glyph during the window, as a share of **one core**.
* **wire KB/min** – bytes actually sent to the client by the server (compressed WebSocket payload).
* **frames/min** – frame messages (full + diff) sent. **refresh ms** – mean time of one server refresh
  (snapshot + decode + render, excluding diff/encode) over the whole run.

## Results – idle and scroll

| profile | page | scenario | Chromium mem MB | (rss sum) | procs | glyph MB | CPU % | wire KB/min | frames/min | refresh ms |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| lean | static | idle | 276 | 858 | 6 | 8 | 0.0 | 0.0 | 0 | 11.5 |
| lean | static | scroll | 245 | 757 | 5 | 9 | 9.6 | 22.4 | 152 | 13.5 |
| lean | dense | idle | 305 | 889 | 6 | 14 | 0.0 | 0.0 | 0 | 116.1 |
| lean | dense | scroll | 275 | 786 | 5 | 15 | 22.1 | 86.0 | 152 | 61.3 |
| lean | ticker | idle | 278 | 856 | 6 | 12 | 8.6 | 4.6 | 233 | 5.8 |
| lean | ticker | scroll | 240 | 747 | 5 | 12 | 19.1 | 5.9 | 233 | 5.6 |
| lean | images | idle | 268 | 848 | 6 | 11 | 0.1 | 0.0 | 0 | 5.1 |
| lean | images | scroll | 236 | 745 | 5 | 13 | 15.3 | 8.7 | 164 | 6.0 |
| balanced | static | idle | 329 | 1068 | 10 | 14 | 0.1 | 0.0 | 0 | 18.6 |
| balanced | static | scroll | 291 | 940 | 8 | 13 | 9.4 | 22.5 | 155 | 12.6 |
| balanced | dense | idle | 350 | 1072 | 8 | 17 | 0.5 | 0.0 | 0 | 75.7 |
| balanced | dense | scroll | 313 | 964 | 7 | 21 | 22.0 | 86.1 | 152 | 58.2 |
| balanced | ticker | idle | 321 | 1038 | 8 | 19 | 9.6 | 5.8 | 304 | 5.8 |
| balanced | ticker | scroll | 284 | 929 | 7 | 17 | 21.8 | 10.6 | 471 | 5.7 |
| balanced | images | idle | 319 | 1038 | 8 | 17 | 0.1 | 0.0 | 0 | 5.4 |
| balanced | images | scroll | 286 | 932 | 7 | 16 | 17.2 | 16.8 | 399 | 5.8 |
| full | static | idle | 327 | 1065 | 10 | 16 | 0.2 | 0.0 | 0 | 12.9 |
| full | static | scroll | 292 | 941 | 8 | 17 | 9.8 | 22.8 | 155 | 12.9 |
| full | dense | idle | 355 | 1074 | 8 | 22 | 0.6 | 0.0 | 0 | 69.0 |
| full | dense | scroll | 317 | 967 | 7 | 23 | 21.9 | 86.6 | 155 | 57.9 |
| full | ticker | idle | 327 | 1064 | 10 | 23 | 9.7 | 5.8 | 304 | 5.6 |
| full | ticker | scroll | 283 | 928 | 8 | 21 | 22.9 | 14.4 | 584 | 5.5 |
| full | images | idle | 323 | 1061 | 10 | 19 | 0.4 | 0.0 | 0 | 4.3 |
| full | images | scroll | 285 | 931 | 8 | 20 | 17.5 | 16.6 | 447 | 5.6 |

Reading it:

* **An untouched page costs nothing to stream**: 0 frames, 0 bytes and 0.0–0.6 % CPU
  on the static, dense and image pages in every profile (that residue is Chromium's own housekeeping).
* **Activity is cheap on the wire.** The ticking page (≈ 5 changes/s) sends 4.6–5.8 KB/min,
  about **19–20 bytes per frame** after diffing and streaming compression, and costs
  9–10 % of a core (see the split below: glyph's own share is about 2 points of that). Lean's 4 fps cap
  sends 233 frames/min instead of 304.
* **Lean uses less memory**: static 16%, dense 13%, ticker 13%, images 16% less than balanced (idle, 20 s window). `balanced` and `full` are
  within 2 % of each other: they differ in what is fetched, not in how much Chromium holds.
* **glyph itself is small**: 8–23 MB, including the harness client.
* **The expensive page is the big DOM**, not the big screen: the 300-row table takes
  58–116 ms per refresh and
  ~22 % CPU while scrolling, against 11–19 ms and
  ~10 % for the article.

## CPU split: glyph vs Chromium

The `(glyph)` column is glyph's own process (server logic, diffing, compression, WebSocket, plus this
harness's trivial client); `CPU %` is the total. Separate 15 s windows, same method:

| profile | page | scenario | Chromium mem MB | (rss sum) | procs | glyph MB | CPU % | (glyph) | wire KB/min | frames/min | refresh ms |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| lean | ticker | idle | 277 | 853 | 6 | 10 | 8.6 | 1.8 | 4.3 | 231 | 5.9 |
| lean | ticker | read | 239 | 745 | 5 | 8 | 17.7 | 1.9 | 6.1 | 235 | 5.6 |
| lean | static | idle | 272 | 855 | 6 | 7 | 0.1 | 0.1 | 0.0 | 0 | 9.5 |
| lean | static | read | 252 | 771 | 5 | 11 | 8.9 | 2.0 | 44.6 | 149 | 11.0 |
| lean | dense | idle | 309 | 915 | 8 | 17 | 0.1 | 0.1 | 0.0 | 0 | 52.9 |
| lean | dense | read | 288 | 828 | 7 | 19 | 22.2 | 3.9 | 130.6 | 149 | 59.0 |
| balanced | ticker | idle | 327 | 1041 | 8 | 21 | 9.7 | 2.4 | 5.7 | 306 | 5.8 |
| balanced | ticker | read | 288 | 933 | 7 | 19 | 20.1 | 3.3 | 10.1 | 436 | 5.5 |
| balanced | static | idle | 321 | 1038 | 8 | 16 | 0.1 | 0.1 | 0.0 | 0 | 11.3 |
| balanced | static | read | 300 | 956 | 7 | 17 | 9.1 | 2.1 | 43.4 | 149 | 11.8 |
| balanced | dense | idle | 362 | 1104 | 10 | 20 | 0.1 | 0.1 | 0.0 | 0 | 65.0 |
| balanced | dense | read | 351 | 1028 | 9 | 22 | 21.0 | 4.0 | 128.4 | 149 | 57.7 |

glyph is **11–24 % of the total CPU** in the active cells; Chromium (repainting, snapshotting, JPEG-encoding
screencast frames, running the page) is the rest. When glyph's CPU matters, it scales with the frame rate and
the size of the DOM snapshot it must parse, not with how much changed on screen.

## Results – reading fresh content

The scroll scenario revisits the same rows, so the streaming compressor has seen every line before and
its wire figure is a **lower bound**. Scrolling steadily through new content costs about twice as much:

| profile | page | scenario | Chromium mem MB | (rss sum) | procs | glyph MB | CPU % | wire KB/min | frames/min | refresh ms |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| lean | static | read | 301 | 916 | 8 | 9 | 8.6 | 42.7 | 149 | 11.6 |
| lean | dense | read | 323 | 921 | 7 | 17 | 19.6 | 128.1 | 149 | 56.1 |
| balanced | static | read | 342 | 1074 | 8 | 15 | 9.0 | 43.7 | 149 | 13.4 |
| balanced | dense | read | 381 | 1115 | 8 | 19 | 20.5 | 126.4 | 149 | 57.7 |
| full | static | read | 342 | 1074 | 8 | 19 | 8.6 | 42.8 | 149 | 11.6 |
| full | dense | read | 382 | 1118 | 8 | 23 | 21.4 | 126.0 | 149 | 56.9 |

That is ≈ 0.7 KB/s of wire traffic for continuous reading of an article (≈ 293 bytes per frame) and
≈ 2.1 KB/s for the dense table (≈ 881 bytes per frame).

## Run-to-run variation

Three repetitions of the 10 s version on static/ticker: memory varied by ±2 % within a profile (lean
300–305 MB, balanced 347–352 MB, static idle), CPU by ±0.5 percentage points, wire bytes by ±1 %. Memory
also depends on how long the page has been alive (renderer processes come and go): the same lean static-idle
cell read 276 MB in the 20 s run and ~303 MB in the 10 s runs. Treat memory as ±10 % across methodologies, ±2 % within one.

## Micro-benchmarks (criterion, `cargo bench`)

200×60 cells for diff/codec; 100×30 recorded pages for render/reader. Median.

```
diff/identical 200x60                                            73.315 µs
diff/3 lines changed                                             77.260 µs
diff/whole screen changed                                        96.986 µs
diff/full frame encode                                           141.69 µs
diff/apply whole-screen diff                                     19.347 µs
codec/encode full frame                                          5.3836 µs
codec/roundtrip full frame                                       15.218 µs
codec/encode small diff                                          289.25 ns
codec/roundtrip small diff                                       554.77 ns
codec/encode whole-screen diff                                   2.4736 µs
codec/roundtrip whole-screen diff                                6.4399 µs
render/parse snapshot json: basic (6 KB)                         39.796 µs
render/decode screenshot jpeg: basic                             500.84 µs
render/cells from snapshot only: basic                           59.520 µs
render/cells from snapshot + pixels: basic                       530.48 µs
render/parse snapshot json: form (5 KB)                          32.110 µs
render/decode screenshot jpeg: form                              489.25 µs
render/cells from snapshot only: form                            63.912 µs
render/cells from snapshot + pixels: form                        531.05 µs
render/parse snapshot json: article (17 KB)                      95.993 µs
render/decode screenshot jpeg: article                           693.26 µs
render/cells from snapshot only: article                         130.49 µs
render/cells from snapshot + pixels: article                     662.05 µs
render/parse snapshot json: overlay (4 KB)                       23.200 µs
render/decode screenshot jpeg: overlay                           516.88 µs
render/cells from snapshot only: overlay                         70.726 µs
render/cells from snapshot + pixels: overlay                     541.68 µs
reader/build document: article                                   69.711 µs
reader/slice viewport (a scroll step)                            36.237 µs
```

(Full output: `bench/results/criterion-macos-arm64.txt`.) These fixtures are small; the next section
shows what happens on a big DOM.

### Where one refresh spends its time

`cargo run --release -p glyph-server --example time_refresh -- bench/pages/dense.html` times the pieces
separately (120×40 cells, mean of 20 refreshes of an unchanging page):

| step | static article (78 KB snapshot) | dense table (824 KB snapshot) |
|---|---:|---:|
| Chromium builds and sends the DOM snapshot | 3.5 ms | **39.2 ms** |
| glyph parses the snapshot JSON | 0.3 ms | 4.2 ms |
| glyph decodes the screenshot JPEG | 1.5 ms | 1.7 ms |
| glyph renders cells | 1.6 ms | 2.3 ms |

So on a large DOM about 83 % of the work is Chromium's snapshot, and on a small page glyph's own
~3.4 ms is about half. The `refresh ms` column in the tables above (58–116 ms for dense) is larger than
the isolated 47 ms because it also includes the screenshot capture, task scheduling and Chromium doing
its own scrolling work at the same time.

## What measuring changed

Several defaults in this repository exist because a number contradicted the plan:

1. **CPU throttling (`Emulation.setCPUThrottlingRate`) is counter-productive.** The brief asked for it in
   the lean profile. Measured on Chrome 154: lean idle CPU was **57 %** of a core at 2× (84 % at 4×), versus
   ~1 % unthrottled – Chromium implements the throttle by duty-cycling the page's main thread. It is off in
   every profile and available as `--cpu-throttle` for experiments.
2. **Chromium's own launch tab costs ~100 MB.** Closing the `about:blank` page it opens (sessions use their
   own browser contexts) took lean from **437 → 308 MB** and balanced from **450 → 355 MB** in quick
   back-to-back runs (static page, idle).
3. **Chromium flags are worth ~10 %, not more.** In-process GPU, ≤ 2 renderers, spare renderer off and site
   isolation off together saved ~45 MB (483 → 437). `--enable-low-end-device-mode`, `--disable-webgl`,
   `--disable-remote-fonts`, `--renderer-process-limit=1` and `--disable-accelerated-2d-canvas` were all
   within noise (±5 MB) and were not adopted.
4. **Measuring memory properly matters.** The first harness reported ~980 MB for the same setup: it
   double-counted a `footprint` summary line, and counted Chromium (glyph's child) inside "glyph's" process.
   Both are fixed and covered by tests; the naive RSS sum is still reported next to the real figure because
   the gap is the point.

## Not measured / caveats

* **Browsh was not benchmarked.** It needs Firefox (not installed) or its Docker image; the Docker daemon
  was not running on the test machine, and the image is linux/amd64-only, so on this Apple-silicon Mac it
  would run under emulation and the numbers would say nothing about Browsh. A fair comparison needs an x86-64
  Linux host with `docker run browsh/browsh`, the same pages served over HTTP, `docker stats` for
  memory/CPU, and a pty reader counting the bytes Browsh writes to its terminal. I did not run this and have
  no Browsh numbers to report.
* **One machine, one OS, one Chrome.** Linux paths (PSS sampling, the watchdog) exist but did not run.
* **Synthetic pages, no network.** Real sites bring scripts, ads, fonts and images; the lean profile's
  blocking matters most exactly there, and this report does not show it. (That blocking works is proven
  separately by `crates/server/tests/profiles.rs`: blocked requests never reach the test server.)
* **No terminal-drawing cost**: the harness client acks and discards. A real terminal's draw time is
  additional, and it is what slows the server through back-pressure on a slow link.
* CPU % is the cumulative CPU time of live processes; Chromium processes that exit during the window take
  their CPU time with them, so it can under-count slightly. Memory is sampled once at the end of each window.

## Reproduce

```sh
cargo build --release
./target/release/glyph bench --seconds 20                  # idle + scroll, 3 profiles, 4 pages
./target/release/glyph bench --scenario read --pages static dense
./target/release/glyph bench --profile lean --pages ticker --json out.json
cargo bench -p glyph-proto --bench codec_diff -- --warm-up-time 1 --measurement-time 3
cargo bench -p glyph-server --bench render    -- --warm-up-time 1 --measurement-time 3
```
