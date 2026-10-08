# glyph

A modern, remote-capable **terminal web browser**. A real Chromium renders the page; glyph turns
it into a grid of terminal cells and streams only what changed to any terminal, local or on
another machine.

```
┌ 1 Docs ×  2 Mail ×  + ──────────────────────────────────────────────┐
│ ▪ https://example.org/docs  — Docs                                   │
│                                                                      │
│  Hello glyph             ← real layout, colours, links, forms        │
│  A paragraph with bold, italic, a link and [ a field_______ ]        │
│                                                                      │
│ NORMAL                                                         42%   │
└──────────────────────────────────────────────────────────────────────┘
```

It is inspired by [Browsh](https://github.com/browsh-org/browsh) (a Go TTY client plus a Firefox
extension) and takes only the *idea*. Browsh is LGPL-2.1; **no Browsh code was read into, copied
or translated into this project**. glyph is written from scratch in Rust around Chromium's
DevTools Protocol, with a different pipeline and a network protocol of its own.

* **Pixel mode** – DOM layout + paint order + screenshot → cells, with truecolor (256/16
  fallback), correct grapheme/wide/emoji handling, box-drawing borders, half-block images, and
  Kitty / iTerm2 / Sixel pictures where the terminal has them.
* **Reader mode** (`M`) – the accessibility tree as a clean, word-wrapped document. The cheapest
  mode: no screencast, no pixels.
* **Remote** – `glyph serve` on one machine, `glyph connect` from any other: cell-diff protocol over
  WebSocket, streaming zstd, back-pressure, TLS, token auth.
* **Cheap on the server** – `lean` / `balanced` / `full` profiles: blocked images/fonts/media/
  trackers, no animations, frame-rate caps, discarded background tabs, no wasted launch tab.
* **A browser UI**: tabs, omnibox (URL or search), vimium-style link hints, find-in-page, mouse,
  forms and text input, selection with OSC 52 copy, help overlay, vi-style rebindable keys, TOML
  config.

## Install

Needs a Chrome or Chromium (≥ 120) on the machine that runs the browser (the *server*; for
`glyph local` that is the same machine). Found automatically, or set `GLYPH_CHROME`.

```sh
# from source (Rust ≥ 1.88)
cargo install --path crates/glyph          # installs the `glyph` binary
# or just build:
cargo build --release                      # target/release/glyph (≈ 6 MB)
```

Pre-built archives for Linux and macOS are produced by `.github/workflows/release.yml` on a version
tag, and a `Dockerfile` and `contrib/glyph.service` (systemd) are provided for servers. Those three
files are **untested** (see [Verification](#what-was-and-was-not-verified)).

## Use

```sh
glyph local https://example.org          # browse here, browser runs in-process
glyph local --profile lean               # cheapest settings

# on a server (loopback, no token needed)
glyph serve
# on a server others can reach: needs a token and TLS
glyph serve --bind 0.0.0.0:7878 --tls-self-signed
#   token: 9c1f…            (generated, printed once)
#   fingerprint: sha256:ab12…
#   connect with: glyph connect wss://<this-host>:7878 --token <token> --fingerprint sha256:ab12…

# from any other machine
glyph connect wss://myserver:7878 --token 9c1f… --fingerprint sha256:ab12…
GLYPH_TOKEN=9c1f… glyph connect wss://myserver:7878 --fingerprint sha256:ab12…
```

Over SSH instead of exposing a port: `ssh -L 7878:127.0.0.1:7878 myserver` then
`glyph connect 127.0.0.1:7878`.

Server knobs worth knowing: `--chrome-arg=--flag` (repeatable, merges `--disable-features`), `--cpu-throttle N`,
`--max-sessions`, `--zstd-level`, `--stats-interval SECS`; env `GLYPH_CHROME`, `GLYPH_CHROME_FLAGS` (e.g.
`--no-sandbox` in containers), `GLYPH_TOKEN`, `GLYPH_LOG_FILE` + `GLYPH_LOG=debug`.

Other commands: `glyph config check|path|default`, `glyph render URL` (draw one frame to stdout),
`glyph dump URL` (page text), `glyph bench` (see [Benchmarks](#benchmarks)).

### Keys (default; `?` shows the live list, everything is rebindable)

| | |
|---|---|
| `j` `k` `h` `l`, arrows | scroll |
| `ctrl-d` `ctrl-u` `space` `ctrl-b` `gg` `G` | half page / page / top / bottom |
| `H` `L`, `r`, `ctrl-c` | back, forward, reload, stop |
| `o`, `O`, `t` | open URL or search / edit current URL / open in new tab |
| `f`, `F`, `gi` | link hints / hints in new tab / focus a text field |
| `ctrl-t`, `x`, `K` `J` (`gt` `gT`), `alt-1…9` | new, close, next/prev, go to tab |
| `/`, `n`, `N` | find (incremental), next, previous |
| `i`, `Esc` | insert mode (keys go to the page) / leave |
| `v`, `y` | keyboard selection / copy it |
| `yy`, `yt` | copy URL / title |
| `M` | toggle reader mode |
| `?`, `ctrl-r`, `ZZ` | help, redraw, quit |

Mouse: click, wheel, middle-click on a link opens a tab, drag selects and copies (OSC 52, so it
works over SSH), double/triple click selects word/line, click the tab bar. Clicking a text field
focuses it and enters insert mode automatically; so does a page that focuses a field itself.

## Configuration

`~/.config/glyph/config.toml` (`$XDG_CONFIG_HOME/glyph/`). Every key is optional and unknown keys
are an error. [`glyph.example.toml`](glyph.example.toml) documents all of them and is tested to be
exactly the defaults. Highlights: `[ui]` search engine, scroll amounts, hint letters, colour depth,
`color_scheme` (what pages see for `prefers-color-scheme`); `[theme]` presets
(`dark`/`light`/`high-contrast`) and any colour; `[images]` protocol; `[keys.normal]` / `[keys.insert]`.

## Resource profiles (`--profile`, server side)

| | lean | balanced (default) | full |
|---|---|---|---|
| images | blocked unless the client can show them | yes | yes |
| fonts / media | blocked | fonts blocked | yes |
| ad & analytics hosts | blocked | blocked | yes |
| favicons | blocked | blocked | – |
| max frame rate | 4 | 10 | 20 |
| screenshot quality | 30 | 45 | 60 |
| CPU throttle | off (see note) | – | – |
| background tabs | closed after 60 s, reloaded on return | frozen | frozen |
| site isolation | off (a security trade-off) | on | on |
| Chromium flags | in-process GPU, ≤ 2 renderers, 256 MB JS heap | – | – |

Note on CPU throttling: the brief asked for `Emulation.setCPUThrottlingRate` in `lean`. Measured on
Chrome 154 it makes an *idle* page burn 57% (2×) to 84% (4×) of a core, because Chromium duty-cycles
the page's main thread, so `lean` leaves it off. `--cpu-throttle N` is there if you want to try it.
The savings come from not doing work instead: frame cap, no animations, nothing sent when idle, and
Chromium's own unused launch tab being closed (≈ −100 MB).

Everywhere: animations/transitions and the text caret are disabled by injected CSS (and
`prefers-reduced-motion`), the page is only re-read when Chromium reports new pixels, and nothing
is sent while a page is idle. Blocking uses `Fetch` interception, so blocked requests never reach
the network. The domain list is short on purpose (`crates/server/src/profile.rs`), not a full
filter list.

## Security

* `serve` binds **127.0.0.1** by default. Any other address **requires a token** and **TLS**
  (`--tls-self-signed`, or `--tls-cert/--tls-key`); `--no-tls` is an explicit opt-out for use behind
  an SSH tunnel or TLS-terminating proxy.
* The token is compared in constant time; failures get a fixed delay and a generic answer; five
  failures lock an IP out for a minute. Handshakes and sessions are capped.
* Browsers cannot hijack a localhost server: connections that carry an `Origin` header are refused
  unless you allow it.
* Not an open proxy. A client can only drive tabs it owns. Navigation is limited to `http`, `https`
  and `about:blank` for remote clients (`file:`, `chrome:`, `javascript:`, `data:` are refused), and
  **remote servers refuse requests to loopback, private and link-local hosts by default**
  (`--allow-private-hosts` to change). Limitation: this checks the host *as written*, so a DNS name
  that resolves to a private address is **not** caught. If that matters, restrict the server's
  network egress.
* Each connected client gets its own Chromium browser context (separate cookies/storage). Clients of
  one server share a Chromium process: do not treat sessions as mutually isolated against a
  determined attacker with a renderer exploit.
* Chromium's debug port is bound to loopback on a random port in a throw-away profile directory. A
  tiny watchdog removes Chromium and the profile if glyph is killed without cleaning up.

## How it works (short)

`DOMSnapshot` gives layout, paint order and text boxes; a screencast gives pixels *and* tells us
when anything changed; the renderer maps both onto an 8×16-px cell grid, painter's-algorithm style
(so a modal hides what is under it), then the server diffs against what the client last received.
[`DESIGN.md`](DESIGN.md) has the algorithm, protocol, back-pressure design and the risks.

## Limitations

* It is a character grid. Layout is approximated; dense small text collides on one row; proportional
  text is slightly longer in cells than in pixels (the last word before a line end can be truncated).
  Reader mode (`M`) is the answer for text-heavy pages.
* Hover-only UI (CSS `:hover` menus) does not open; pointer-move events are not forwarded.
* No extensions, DRM, password manager, mobile emulation, printing, downloads, or file uploads.
* Right-to-left text is shown in logical order (a cell grid has no bidi).
* Graphics protocols are only used outside tmux/screen unless forced, and only as well as the
  terminal supports them.
* Browsh comparison: not run (see [Benchmarks](#benchmarks)).

## Benchmarks

Measured on an Apple M4 / Chrome 154 with the bundled sample pages (details, method and caveats in
[`bench/REPORT.md`](bench/REPORT.md)):

| | lean | balanced |
|---|---:|---:|
| Chromium memory, one tab, idle (shared-aware) | 270–310 MB | 320–360 MB |
| glyph process | 7–20 MB | 14–22 MB |
| untouched page: bytes sent / CPU | 0 B/min / ≈ 0 % | 0 B/min / ≈ 0 % |
| page changing 5×/s: wire / CPU | 4.6 KB/min / 9 % | 5.8 KB/min / 10 % |
| reading an article (fresh content): wire | ≈ 0.7 KB/s | ≈ 0.7 KB/s |

Honest bits: Chromium itself is the cost floor (its process tree is most of the memory and ~80 % of the
CPU); `lean` buys ~13–16 % memory, not a different order of magnitude. `Emulation.setCPUThrottlingRate`,
which the original brief asked for, *raised* idle CPU to 57–84 % in testing, so it is off. **No Browsh
comparison was run** (no usable Docker/Firefox here; the report explains what a fair one needs).

Reproduce with:

```sh
cargo build --release
./target/release/glyph bench --seconds 20          # all profiles × 4 pages × idle+scroll
./target/release/glyph bench --profile lean --pages ticker --json out.json
cargo bench                                          # criterion: diff, codec, render, reader mode
```

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace            # Chromium tests run if a Chrome is found, otherwise print SKIP
cargo run -p glyph-server --example record   # re-record fixtures/recorded from live Chromium
INSTA_UPDATE=always cargo test -p glyph-server --test render_fixtures   # re-bless snapshots
```

Layout: `crates/proto` (cells, grid, diff, messages, codec) · `crates/server` (CDP client, renderer,
reader mode, profiles, sessions, WebSocket/TLS) · `crates/client` (TUI, keymap, hints, graphics) ·
`crates/glyph` (the binary, benchmark harness) · `fixtures/` (HTML + recorded Chromium output).

### What was and was not verified

Verified by running it (macOS arm64, Chrome 154): the full test suite (unit, property, insta
snapshots of rendered frames, loopback-WebSocket end-to-end incl. TLS pinning/auth/back-pressure,
live-Chromium sessions), the TUI driven in a pseudo-terminal, a TLS `serve`/`connect` session against
the public web, and the benchmark harness.

**Not** verified: Kitty / iTerm2 / Sixel output in a real terminal that supports them (only the bytes
are tested, including a Sixel round-trip decoder); Windows; Linux (code is portable, memory
sampling has a Linux branch, none of it ran); the Dockerfile; the systemd unit; the GitHub Actions
workflows (YAML parsed, never executed); a comparison against Browsh.

## License

MIT or Apache-2.0, at your option.
