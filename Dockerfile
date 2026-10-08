# glyph server in a container:  docker build -t glyph . && docker run --rm -p 7878:7878 -e GLYPH_TOKEN=change-me glyph
#
# NOTE: Chromium's sandbox needs kernel features that containers usually withhold, so this image
# runs it with --no-sandbox. Treat the container itself as the sandbox: do not run it privileged,
# keep it on a network that cannot reach anything private, and see "Security" in the README.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p glyph

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends chromium ca-certificates tini \
      fonts-noto-core fonts-noto-cjk fonts-noto-color-emoji \
 && apt-get clean \
 && useradd --create-home --uid 10001 glyph
COPY --from=build /src/target/release/glyph /usr/local/bin/glyph
USER glyph
ENV GLYPH_CHROME=/usr/bin/chromium GLYPH_CHROME_FLAGS=--no-sandbox
EXPOSE 7878
# Remote binds need a token (GLYPH_TOKEN) and TLS. This uses a throw-away self-signed certificate;
# its fingerprint is printed at start-up, and clients pin it with `--fingerprint`.
ENTRYPOINT ["/usr/bin/tini", "--", "glyph", "serve", "--bind", "0.0.0.0:7878", "--tls-self-signed", "--profile", "lean"]
