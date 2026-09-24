# Mirror Frontend: single-document code and operations guide

This document describes the current repository so an agent can understand, run, and modify it without first reading source. Mirror Frontend is a single-process Rust HTTP reverse proxy for **one configured upstream hostname**. Clients address this server; it forwards their requests over HTTPS to that hostname, then rewrites upstream URLs in selected responses so links stay on the public proxy address (or an optional shadow domain). It does not crawl, cache, store, or render a site.

## Repository map

| File | Role |
| --- | --- |
| [Cargo.toml](./Cargo.toml), [Cargo.lock](./Cargo.lock) | Rust 2024 package, dependencies and lockfile. |
| [src/main.rs](./src/main.rs) | Entire server: configuration, routing, forwarding, rewriting, logging, and unit/integration tests. There is no library crate. |
| [.env.example](./.env.example) | Example local configuration; actual `.env` is ignored by Git. |
| [Dockerfile](./Dockerfile), [.dockerignore](./.dockerignore) | Multistage container build and build-context exclusions. |
| [.github/workflows/publish-container.yml](./.github/workflows/publish-container.yml) | Container publication on version tags. |
| [README.md](./README.md) | Short project overview and invocation examples. |

Runtime stack: Axum 0.8 handles HTTP, Tokio 1 runs the async server, Reqwest 0.12 sends upstream requests with Rustls TLS and streaming support, `url` parses/constructs URLs, `dotenvy` reads `.env`, and `humantime` formats log timestamps. Tests use Tower's `ServiceExt`. There is no database, frontend bundle, or external service other than the configured upstream.

## Configuration and startup

Start locally with Rust/Cargo installed:

```sh
cp .env.example .env
# Edit .env: set MIRROR_UPSTREAM to the desired hostname.
cargo run
```

Environment variables already present in the process take precedence over `.env`. Missing `.env` is allowed; malformed or unreadable `.env` fails startup. The server logs startup failures to stderr and exits with status 1. All configuration is read once at startup; changes require restart.

| Variable | Meaning | Default / validation |
| --- | --- | --- |
| `MIRROR_UPSTREAM` | Only destination to which requests are forwarded. | Required hostname only, such as `example.com`. No scheme, port, path, query, fragment, user info, whitespace, or trailing slash. Upstream URL is `https://<hostname>/`. |
| `SHADOW_DOMAIN` | Origin used for URL replacement in response headers and text bodies. Does **not** change upstream target or incoming request routing. | Unset/empty: derive public origin from each request. Otherwise same clean-hostname validation as `MIRROR_UPSTREAM`; replacements use HTTPS. |
| `MIRROR_BIND` | TCP listen address. | `0.0.0.0:3000`; must parse as a socket address. |
| `DEBUG` | Emit startup and per-request logs. | `false`; only literal lowercase `true` or `false` accepted. |

For example, with upstream `example.com` and bind `127.0.0.1:3000`, a request for `http://localhost:3000/path?q=1` targets `https://example.com/path?q=1`. A public reverse proxy may terminate HTTPS and forward to this HTTP listener. Its `X-Forwarded-Host` and `X-Forwarded-Proto` values determine the public origin used in response rewriting unless `SHADOW_DOMAIN` is set. Do not expose this listener to clients that can forge those headers if their influence on generated links matters; there is no trusted-proxy validation in the application.

## Request-to-response flow

```text
client -> Axum catch-all -> public URL + target URL -> filter/rewrite request headers
       -> Reqwest HTTPS upstream (request body streamed)
       -> filter/rewrite response headers -> buffer/rewrite eligible text OR stream other bodies
       -> client
```

1. `main` calls `run`, which loads `.env`, validates configuration, constructs one shared Reqwest client with automatic redirects disabled, binds TCP, and serves the Axum router with socket connection metadata. The router has one fallback handler for **all paths and HTTP methods**; there are no dedicated API, health, or static-file routes.
2. `proxy` reads client IP from socket metadata (or `unknown` when absent) and records the URL path for optional logging. It derives a public base URL from the first comma-separated `X-Forwarded-Host` value, falling back to `Host`; likewise `X-Forwarded-Proto` supplies `http` or `https`, defaulting to `http`. Missing/invalid host or invalid forwarded protocol returns HTTP 400. These forwarded values are trusted as provided.
3. `target_url` clones the configured upstream URL and attaches the incoming request path and query. `proxy` preserves the incoming HTTP method and streams its body to Reqwest. It strips hop-by-hop headers, headers named in `Connection`, `Host`, and incoming `Content-Length`; Reqwest supplies the destination host. It replaces occurrences of the public origin in `Origin` and `Referer` with the upstream origin, then sets `Accept-Encoding: identity`.
4. Reqwest does **not** follow upstream redirects. `build_response` carries the upstream status through unchanged. It filters response hop-by-hop headers and `Host`. For rewritten bodies it also removes `Content-Length`; otherwise it retains that header. It rewrites selected response header values and removes exact-upstream cookie `Domain` attributes as described below.
5. If `Content-Type` is eligible **and** there is no `Content-Encoding` header, `build_response` reads the **entire** upstream body into memory, performs UTF-8 string replacement, and returns those bytes. Invalid UTF-8 stays byte-for-byte unchanged but still takes this buffered path. All other bodies stream as chunks without content rewriting. Upstream send/body-read errors return HTTP 502 (`upstream request failed`). An error while streaming a non-rewritten body may occur after response headers were sent, not as a new 502.

### Rewrite rules

Eligible response types: any `text/*`, plus `application/javascript`, `application/json`, `application/manifest+json`, `application/xhtml+xml`, `application/xml`, and `image/svg+xml`. The check removes MIME parameters such as `charset`, but type matching is otherwise literal. Compressed responses with `Content-Encoding` are not rewritten even when their type qualifies. Requesting `Accept-Encoding: identity` encourages uncompressed upstream responses but does not guarantee them.

Text rewriting is plain, case-sensitive substring replacement, **not** HTML parsing or URL resolution. It replaces `https://`, `http://`, `ftp://`, and `rsync://` followed by the upstream authority with the chosen destination origin (including its scheme/port), then replaces protocol-relative `//<upstream authority>` with `//<destination authority>`. Relative paths, escaped/encoded hostnames, arbitrary script-built URLs, and other representations are not resolved. This is not a transparent general-purpose web proxy; content rewrites can change unrelated matching text and do not parse URL boundaries.

Response headers rewritten using the same text function: `Location`, `Content-Location`, `Link`, `Refresh`, `Access-Control-Allow-Origin`, and `Content-Security-Policy`. Repeated header values are preserved. `Set-Cookie` is handled separately: a `Domain` attribute exactly matching the upstream hostname (case-insensitive, optionally preceded by a dot) is removed, allowing a browser to treat that cookie as host-only for the proxy host. Other cookie attributes and domains pass through. Setting `SHADOW_DOMAIN` does not rewrite cookie domains to the shadow host.

### Errors and logging

| Situation | Result |
| --- | --- |
| Invalid/missing `Host`, invalid public URL, invalid `X-Forwarded-Proto` | HTTP 400, plain-text cause. |
| Reqwest upstream send failure or buffered body read failure | HTTP 502, `upstream request failed`. |
| Valid upstream response, including 3xx/4xx/5xx | Same HTTP status, with applicable header/body rewrites. |
| Configuration, HTTP client creation, or listener failure | Startup error on stderr and process exits with status 1. |

When `DEBUG=true`, startup logs the bind address. Each completed handler invocation logs UTC RFC 3339 time, severity (`INFO` for non-4xx/5xx, `WARN` for 4xx, `ERR` for 5xx), socket client IP, path, and status. Query strings are not logged. Startup errors are logged even when debug is disabled. The socket IP is not replaced with `X-Forwarded-For`; behind a reverse proxy it can be the proxy IP. The code has no access log for a failure that occurs during a streamed response after handler return.

## Build, test, and deployment

```sh
cargo check --locked
cargo test --locked
docker build -t mirror-frontend .
docker run --rm -e MIRROR_UPSTREAM=example.com -p 3000:3000 mirror-frontend
```

The Dockerfile builds a locked release binary in a Rust Bookworm builder, copies only that binary into Debian Bookworm slim, and runs as UID/GID 65532. Supply configuration via environment variables when running a container; `.env` is excluded from the build context. For host access, publish the bind port; for another container, place both on the same Docker network and address the container name/port. The Docker build targets Linux AMD64 in CI.

The GitHub Actions workflow triggers on pushed tags matching `v*`, uses Docker Buildx and GitHub Container Registry, and publishes `latest`, the semantic version, major.minor, and a commit-derived tag for `linux/amd64`. The image name derives from the repository name. This is publication behavior, not a command to run locally.

Tests live at the end of `src/main.rs`. They cover debug parsing/status categories, clean-host validation, hop-by-hop header removal, URL and response-header rewrites, forwarded public-origin derivation, missing-Host rejection, and a local HTTP upstream exercising method/path/query/body forwarding and response rewrite (with and without shadow domain). The local upstream in that test is directly injected into application state; normal startup still requires an HTTPS hostname. There is no separate end-to-end TLS, Docker, or public reverse-proxy test.

## Modification guide and boundaries

- Configuration changes belong in `run` and its parsers; update `.env.example`, documented defaults, and validation tests together. The only configured destination is `MIRROR_UPSTREAM`; do not turn untrusted request data into a new upstream target.
- Request routing and forwarding live in `app`, `proxy`, `target_url`, `request_public_url`, `filtered_headers`, and `rewrite_request_headers`. `SHADOW_DOMAIN` is response-only; request `Origin`/`Referer` rewrites use the actual public request URL.
- Response behavior lives in `build_response`, `is_rewritable`, `rewrite_response_headers`, `rewrite_cookies`, and `rewrite_text`. If changing URL rewriting, check **both** response headers and body paths, along with cookie semantics, content encoding, and `Content-Length` handling.
- `ProxyError` maps request failures to 400 and upstream failures to 502. `LogCategory` and `log` own severity/timestamp formatting. Keep error and debug semantics aligned with these paths.
- Extend the existing tests in `src/main.rs` for behavior changes; run `cargo test --locked`. Avoid assuming browser navigation stays on the proxy for every site: only listed headers and eligible plain-text bodies are rewritten; redirects remain visible rather than followed.