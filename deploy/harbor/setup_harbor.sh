#!/usr/bin/env bash
# Stage 1 — Harbor installation (reproducible / Infrastructure-as-Code).
#
# Downloads a *pinned* Harbor offline installer (verified against a known
# sha256), renders harbor.yml from the shipped template with our settings and
# runs the installer with the Trivy scanner.
#
# HTTP-only ("sandbox") is a deliberate choice here, but it is not free: every
# Docker daemon that talks to this registry (dev laptops, the VPS, CI runners)
# must list it under `insecure-registries` in /etc/docker/daemon.json and be
# restarted, and oras/buildx need `--plain-http`. See HARBOR_GUIDE.md. For
# anything beyond a throwaway LAN sandbox, put a TLS-terminating proxy in front
# or switch this template to `https`.
#
# Idempotent: re-running re-renders harbor.yml and `docker compose up -d` in
# the existing install dir; the archive is only downloaded once.
set -euo pipefail

# ------------------------------------------------------------------ settings
# Pin the version — "latest" is not reproducible. Bump deliberately and update
# the checksum from the official release's *.asc/sha256 on GitHub.
HARBOR_VERSION="${HARBOR_VERSION:-v2.11.1}"
HARBOR_SHA256="${HARBOR_SHA256:-}" # optional; when set, the archive is verified

# Rendered into harbor.yml. HARBOR_HOSTNAME must match how clients reach the
# registry (it is baked into image references and tokens) — an IP or DNS name,
# never "localhost" if anything remote pulls from it.
HARBOR_HOSTNAME="${HARBOR_HOSTNAME:-harbor.local}"
HARBOR_HTTP_PORT="${HARBOR_HTTP_PORT:-80}"
HARBOR_ADMIN_PASSWORD="${HARBOR_ADMIN_PASSWORD:-Harbor12345ChangeMe}"
HARBOR_DATA_VOLUME="${HARBOR_DATA_VOLUME:-/data/harbor}"
# Where to unpack the installer.
HARBOR_INSTALL_DIR="${HARBOR_INSTALL_DIR:-/opt/harbor}"

# ------------------------------------------------------------------- helpers
log() { printf '==> %s\n' "$*"; }
die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

command -v docker >/dev/null || die "docker is required"
docker compose version >/dev/null 2>&1 || die "docker compose v2 is required"
command -v curl >/dev/null || die "curl is required"

ARCHIVE="harbor-offline-installer-${HARBOR_VERSION}.tgz"
URL="https://github.com/goharbor/harbor/releases/download/${HARBOR_VERSION}/${ARCHIVE}"
WORK="${HARBOR_INSTALL_DIR%/harbor}"

# ------------------------------------------------------------------ download
sudo mkdir -p "${WORK}"
if [[ ! -d "${HARBOR_INSTALL_DIR}" ]]; then
  if [[ ! -f "/tmp/${ARCHIVE}" ]]; then
    log "Downloading Harbor ${HARBOR_VERSION}"
    curl -fL --retry 3 -o "/tmp/${ARCHIVE}" "${URL}"
  fi
  if [[ -n "${HARBOR_SHA256}" ]]; then
    log "Verifying checksum"
    echo "${HARBOR_SHA256}  /tmp/${ARCHIVE}" | sha256sum -c - || die "checksum mismatch"
  else
    log "HARBOR_SHA256 not set — skipping checksum verification (set it for real IaC)"
  fi
  log "Extracting into ${WORK}"
  sudo tar -xzf "/tmp/${ARCHIVE}" -C "${WORK}"
else
  log "Install dir ${HARBOR_INSTALL_DIR} already exists — reusing"
fi

# ------------------------------------------------------------- render config
log "Rendering harbor.yml (hostname=${HARBOR_HOSTNAME}, http port=${HARBOR_HTTP_PORT})"
sudo mkdir -p "${HARBOR_DATA_VOLUME}"
TEMPLATE="${HARBOR_INSTALL_DIR}/harbor.yml.tmpl"
OUT="${HARBOR_INSTALL_DIR}/harbor.yml"
[[ -f "${TEMPLATE}" ]] || die "template ${TEMPLATE} not found — bad archive?"

# Start from the template, then set the fields we manage. The template ships
# with an `https:` block that must be removed for HTTP-only, otherwise the
# installer demands a cert.
sudo cp "${TEMPLATE}" "${OUT}"
sudo python3 - "${OUT}" "${HARBOR_HOSTNAME}" "${HARBOR_HTTP_PORT}" \
  "${HARBOR_ADMIN_PASSWORD}" "${HARBOR_DATA_VOLUME}" <<'PY'
import re, sys
path, hostname, http_port, admin_pw, data_vol = sys.argv[1:6]
text = open(path).read()

text = re.sub(r'(?m)^hostname:.*$', f'hostname: {hostname}', text)
text = re.sub(r'(?m)^  port:\s*80\s*$', f'  port: {http_port}', text, count=1)
text = re.sub(r'(?m)^harbor_admin_password:.*$',
              f'harbor_admin_password: {admin_pw}', text)
text = re.sub(r'(?m)^data_volume:.*$', f'data_volume: {data_vol}', text)

# Drop the https block (4 lines: `https:` + certificate/private_key/port) so
# HTTP-only installs don't require a TLS cert.
text = re.sub(r'(?m)^https:\n(?:[ ]+.*\n)+', '', text)

open(path, 'w').write(text)
print("harbor.yml rendered")
PY

# ---------------------------------------------------------------- install
log "Running installer (with Trivy)"
(cd "${HARBOR_INSTALL_DIR}" && sudo ./install.sh --with-trivy)

log "Done. Harbor UI/API: http://${HARBOR_HOSTNAME}:${HARBOR_HTTP_PORT}"
log "Next: HARBOR_URL=http://${HARBOR_HOSTNAME}:${HARBOR_HTTP_PORT} \\"
log "      HARBOR_ADMIN_PASSWORD=... ./configure_harbor_api.sh"
