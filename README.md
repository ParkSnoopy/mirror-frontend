# Mirror Frontend

Mirror Frontend exposes one configured HTTPS website through your own server. Incoming paths, queries, methods, request bodies, and response bodies pass through this server. Redirects and absolute URLs in text responses are rewritten to keep browsing on the proxy address. Large binary downloads are streamed without buffering.

## Run

Copy the example configuration, edit `.env`, then start the server:

```sh
cp .env.example .env
cargo run --release
```

Open the server through any public hostname. Mirror Frontend derives that address from each request and uses `X-Forwarded-Host` and `X-Forwarded-Proto` when an HTTPS reverse proxy supplies them. `MIRROR_UPSTREAM` accepts only a hostname such as `mirror.example.com`; schemes, ports, paths, and trailing slashes are rejected. Upstream requests use HTTPS. Existing environment variables override matching `.env` values.

Optional settings:

- `MIRROR_BIND`: listening address. Default: `0.0.0.0:3000`.

Only the configured destination is reachable. This is not an open forward proxy.

## Docker

Pass the destination and listening address directly when starting the published image.

### Host access

```sh
docker run --rm --name mirror-frontend \
  -e MIRROR_UPSTREAM=example.com \
  -e MIRROR_BIND=0.0.0.0:8080 \
  -p 8080:8080 \
  ghcr.io/parksnoopy/mirror-frontend:latest
```

The host can use `http://localhost:8080`.

### Docker network

```sh
docker run --rm --name mirror-frontend \
  --network app-network \
  -e MIRROR_UPSTREAM=example.com \
  -e MIRROR_BIND=0.0.0.0:8080 \
  ghcr.io/parksnoopy/mirror-frontend:latest
```

Other containers on `app-network` can use `http://mirror-frontend:8080`.

`MIRROR_UPSTREAM` is required; the container exits when it is missing. `MIRROR_BIND` selects any listening address and port. Images support `linux/amd64`. Pushing a version tag such as `v1.2.3` publishes `latest`, `1.2.3`, `1.2`, and a commit tag.
