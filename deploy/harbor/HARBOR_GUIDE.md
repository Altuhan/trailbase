# Harbor for TrailBase — CI/CD & OCI artifacts

Harbor as container registry, OCI artifact store (WASM plugins for the Wasmtime
runtime) and pull-through cache for CI. This guide assumes the two scripts in
this directory have run:

```bash
HARBOR_HOSTNAME=harbor.local HARBOR_ADMIN_PASSWORD='…' ./setup_harbor.sh
HARBOR_URL=http://harbor.local HARBOR_ADMIN_PASSWORD='…' ./configure_harbor_api.sh
```

`configure_harbor_api.sh` prints a `robot$ci` secret once — store it in your CI
secrets and use it instead of the admin password below.

## 0. HTTP-only registries: the insecure-registries tax

The sandbox runs Harbor over plain HTTP. Every Docker daemon that pushes/pulls
must be told to trust it, or you'll get `http: server gave HTTP response to
HTTPS client`:

```json
// /etc/docker/daemon.json — then: sudo systemctl restart docker
{ "insecure-registries": ["harbor.local", "harbor.local:80"] }
```

`oras` and `buildx` take `--plain-http` instead (shown below). For anything
past a LAN sandbox, front Harbor with TLS and delete this section.

## 1. Build & push the TrailBase image

The repo's root `Dockerfile` builds a static `trail` (musl) with the embedded
MCP server:

```bash
# from the repo root, on a machine with Docker
docker build -t harbor.local/trailbase-prod/trailbase:0.30.1-mcp .

echo "$ROBOT_SECRET" | docker login harbor.local -u 'robot$ci' --password-stdin
docker push harbor.local/trailbase-prod/trailbase:0.30.1-mcp
```

`trailbase-prod` blocks pulls of images scanning at **medium+** severity, so a
failed `docker pull` on the staging host means "fix the CVEs or push to
`trailbase-sandbox` instead" — not a registry error.

## 2. Push a WASM plugin as an OCI artifact (oras)

TrailBase loads WASM components (Wasmtime) from `traildepot/wasm/`. Harbor
stores them as OCI artifacts. Using the repo's real component as the example:

```bash
# build the component (same as the Dockerfile's auth-ui-builder stage)
rustup target add wasm32-wasip2
cargo build --target wasm32-wasip2 --release -p trailbase-auth-ui-component
WASM=target/wasm32-wasip2/release/trailbase_auth_ui_component.wasm

# push it with an explicit artifact type so it's not mistaken for an image
oras push --plain-http \
  harbor.local/trailbase-sandbox/wasm-auth-ui:0.30.1 \
  --artifact-type application/vnd.wasm.component.v1+wasm \
  "${WASM}:application/wasm"
```

Pull it back on the target host into the depot:

```bash
oras pull --plain-http harbor.local/trailbase-sandbox/wasm-auth-ui:0.30.1 \
  -o /srv/trailbase/traildepot/wasm/
```

Note: Trivy scans container images, not arbitrary WASM artifacts — the scan
overview will be empty for these, which is expected.

## 3. Use the proxy cache in CI

Pull base images through `docker-hub-proxy` instead of Docker Hub directly, so
CI stops hitting Hub's anonymous rate limit. Just rewrite the reference:

```dockerfile
# before
FROM alpine:3.23
# after
FROM harbor.local/docker-hub-proxy/library/alpine:3.23
```

buildx over plain HTTP:

```bash
docker buildx build \
  --build-arg BASE=harbor.local/docker-hub-proxy/library/alpine:3.23 \
  --output=type=registry,registry.insecure=true \
  -t harbor.local/trailbase-sandbox/app:ci .
```

**GitHub Actions caveat:** GitHub-hosted runners cannot reach a Harbor on your
LAN. Either run a **self-hosted runner** on the same network, or limit CI usage
of this registry to local Docker / on-prem runners. A hosted-runner workflow
pointing at `harbor.local` will simply time out.

## Acceptance checklist

- [ ] `docker login harbor.local` with the robot secret succeeds.
- [ ] Push to `trailbase-sandbox` → artifact gets a Trivy scan overview.
- [ ] Pull a known-vulnerable image from `trailbase-prod` is **denied**.
- [ ] `oras pull` of the WASM artifact returns the `.wasm` file.
- [ ] A build `FROM harbor.local/docker-hub-proxy/library/...` succeeds and the
      image appears under the `docker-hub-proxy` project.
