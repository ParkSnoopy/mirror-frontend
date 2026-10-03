# Mirror Frontend

Mirror Frontend exposes one configured HTTPS website through your own server. Incoming paths, queries, methods, request bodies, and response bodies pass through this server. Redirects and absolute URLs in text responses are rewritten to keep browsing on the proxy address. Large binary downloads are streamed without buffering.

For code structure, request flow, configuration, and maintenance, see [LLM_WIKI.md](./LLM_WIKI.md).

## Run

Copy the example configuration, edit `.env`, then start the server:

```sh
cp .env.example .env
cargo run --release
```

Open the server through any public hostname. Mirror Frontend derives that address from each request and uses `X-Forwarded-Host` and `X-Forwarded-Proto` when an HTTPS reverse proxy supplies them. `MIRROR_UPSTREAM` accepts only a hostname such as `mirror.example.com`; schemes, ports, paths, and trailing slashes are rejected. Upstream requests use HTTPS. Existing environment variables override matching `.env` values.

Optional settings:

- `MIRROR_BIND`: listening address. Default: `0.0.0.0:3000`.
- `SHADOW_DOMAIN`: clean hostname used when rewriting URLs in response content and response headers. Rewrites use `https://SHADOW_DOMAIN`; leave it empty to use each request's public address. The upstream request target remains `MIRROR_UPSTREAM`.
- `DEBUG`: set to `true` for terminal request logs or `false` to disable them. Default: `false`.
- `GOOGLE_FAIL`: accepts only `403`, `404`, or `500`. Recommended: `404`, enabled in [.env.example](./.env.example). Intercepted requests return only the selected status and its standard plain-text message (`Forbidden`, `Not Found`, or `Internal Server Error`), without naming Google or explaining interception. Rewritten URLs use a neutral local path; no Google request is sent. Omit to disable; an empty or unsupported value fails startup. Domains are maintained in [google-fail-domains.txt](./google-fail-domains.txt), one per line including subdomains; rebuild after editing. The initial list covers `googleapis.com` and `gstatic.com`.

Debug logs include a UTC timestamp, `INFO`, `WARN`, or `ERR` category, client IP address, request path, and response status. Query strings are omitted.

Only the configured destination is reachable. This is not an open forward proxy.

## Docker

Pass the destination and listening address directly when starting the published image.

### Host access

```sh
docker run --rm --name mirror-frontend \
  -e MIRROR_UPSTREAM=example.com \
  -e SHADOW_DOMAIN=shadow.example.com \
  -e MIRROR_BIND=0.0.0.0:8080 \
  -e DEBUG=false \
  -p 8080:8080 \
  ghcr.io/parksnoopy/mirror-frontend:latest
```

The host can use `http://localhost:8080`.

### Docker network

```sh
docker run --rm --name mirror-frontend \
  --network app-network \
  -e MIRROR_UPSTREAM=example.com \
  -e SHADOW_DOMAIN=shadow.example.com \
  -e MIRROR_BIND=0.0.0.0:8080 \
  -e DEBUG=false \
  ghcr.io/parksnoopy/mirror-frontend:latest
```

Other containers on `app-network` can use `http://mirror-frontend:8080`.

`MIRROR_UPSTREAM` is required; the container exits when it is missing. `MIRROR_BIND` selects any listening address and port. Images support `linux/amd64`. Pushing a version tag such as `v1.2.3` publishes `latest`, `1.2.3`, `1.2`, and a commit tag.
