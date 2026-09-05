# Mirror Frontend

Mirror Frontend exposes one configured HTTP or HTTPS website through your own server. Incoming paths, queries, methods, request bodies, and response bodies pass through this server. Redirects and absolute URLs in text responses are rewritten to keep browsing on the proxy address. Large binary downloads are streamed without buffering.

## Run

Copy the example configuration, edit `.env`, then start the server:

```sh
cp .env.example .env
cargo run --release
```

Open the server through any public hostname. Mirror Frontend derives that address from each request and uses `X-Forwarded-Host` and `X-Forwarded-Proto` when an HTTPS reverse proxy supplies them. A destination hostname without a scheme uses HTTPS. Existing environment variables override matching `.env` values.

Optional settings:

- `MIRROR_BIND`: listening address. Default: `0.0.0.0:3000`.

Only the configured destination is reachable. This is not an open forward proxy.

## Docker

Pass the destination directly when starting the published image:

```sh
docker run --rm --name mirror-frontend \
  -e MIRROR_UPSTREAM=https://example.com \
  -p 3000:3000 \
  ghcr.io/parksnoopy/mirror-frontend:latest
```

`MIRROR_UPSTREAM` is required; the container exits when it is missing. Images support `linux/amd64` and `linux/arm64`. Pushing a version tag such as `v1.2.3` publishes `latest`, `1.2.3`, `1.2`, and a commit tag.
