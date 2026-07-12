#!/usr/bin/env bash
# Stage 2 — Harbor project & policy configuration via the REST API.
#
# Idempotent: every create tolerates "already exists" (HTTP 409) so re-runs are
# safe. Uses admin basic-auth to bootstrap; for CI, prefer the robot account
# this script prints at the end over the admin password.
#
# Creates:
#   - project trailbase-sandbox : public,  auto-scan on push
#   - project trailbase-prod    : private, block pull of vulnerable images (>= medium)
#   - registry endpoint + project docker-hub-proxy : pull-through cache for Docker Hub
#   - robot account robot$ci    : pull/push on both trailbase projects
#   - tag retention "keep last 10" on both trailbase projects
set -euo pipefail

# ------------------------------------------------------------------ settings
HARBOR_URL="${HARBOR_URL:-http://harbor.local}"          # no trailing slash
HARBOR_ADMIN_USER="${HARBOR_ADMIN_USER:-admin}"
HARBOR_ADMIN_PASSWORD="${HARBOR_ADMIN_PASSWORD:?set HARBOR_ADMIN_PASSWORD}"
# Optional Docker Hub creds for the proxy cache — anonymous pull-through still
# shares Docker Hub's anonymous rate limit, so set these to actually lift it.
DOCKERHUB_USERNAME="${DOCKERHUB_USERNAME:-}"
DOCKERHUB_PASSWORD="${DOCKERHUB_PASSWORD:-}"

API="${HARBOR_URL%/}/api/v2.0"
AUTH=(-u "${HARBOR_ADMIN_USER}:${HARBOR_ADMIN_PASSWORD}")

command -v curl >/dev/null || { echo "curl is required" >&2; exit 1; }
command -v jq >/dev/null || { echo "jq is required" >&2; exit 1; }

log() { printf '==> %s\n' "$*"; }

# Wait until the API is reachable (installer may still be warming up).
log "Waiting for Harbor API at ${API}"
for i in $(seq 1 60); do
  if curl -fsS "${AUTH[@]}" "${API}/health" >/dev/null 2>&1; then break; fi
  [[ $i -eq 60 ]] && { echo "Harbor API not reachable" >&2; exit 1; }
  sleep 2
done

# POST that treats 201 (created) and 409 (already exists) as success; any other
# status prints the body and fails.
api_post() {
  local path="$1" body="$2" code
  code="$(curl -s -o /tmp/harbor_resp -w '%{http_code}' "${AUTH[@]}" \
    -X POST -H 'Content-Type: application/json' -d "${body}" "${API}${path}")"
  case "${code}" in
    200|201) return 0 ;;
    409) echo "    (already exists)"; return 0 ;;
    *) echo "    POST ${path} -> HTTP ${code}: $(cat /tmp/harbor_resp)" >&2; return 1 ;;
  esac
}

project_id() {
  curl -fsS "${AUTH[@]}" "${API}/projects?name=$1" | jq -r '.[0].project_id // empty'
}

# ----------------------------------------------------------- sandbox project
log "Project trailbase-sandbox (public, auto-scan on push)"
api_post "/projects" '{
  "project_name": "trailbase-sandbox",
  "public": true,
  "metadata": { "public": "true", "auto_scan": "true" }
}'

# -------------------------------------------------------------- prod project
log "Project trailbase-prod (private, prevent vulnerable pulls >= medium)"
api_post "/projects" '{
  "project_name": "trailbase-prod",
  "public": false,
  "metadata": {
    "public": "false",
    "auto_scan": "true",
    "prevent_vul": "true",
    "severity": "medium"
  }
}'

# --------------------------------------------------- docker hub proxy cache
log "Registry endpoint for Docker Hub"
if [[ -n "${DOCKERHUB_USERNAME}" ]]; then
  CRED="{\"access_key\":\"${DOCKERHUB_USERNAME}\",\"access_secret\":\"${DOCKERHUB_PASSWORD}\",\"type\":\"basic\"}"
else
  CRED='null'
  echo "    (no DOCKERHUB_USERNAME — anonymous proxy still shares Docker Hub's anon rate limit)"
fi
# NB: the registry URL is the Docker *registry* endpoint, not hub.docker.com.
api_post "/registries" "{
  \"name\": \"dockerhub\",
  \"type\": \"docker-hub\",
  \"url\": \"https://registry-1.docker.io\",
  \"credential\": ${CRED},
  \"insecure\": false
}"

REG_ID="$(curl -fsS "${AUTH[@]}" "${API}/registries?q=name%3Ddockerhub" | jq -r '.[0].id // empty')"
if [[ -n "${REG_ID}" ]]; then
  log "Project docker-hub-proxy (proxy cache -> registry ${REG_ID})"
  api_post "/projects" "{
    \"project_name\": \"docker-hub-proxy\",
    \"public\": true,
    \"registry_id\": ${REG_ID},
    \"metadata\": { \"public\": \"true\" }
  }"
else
  echo "    WARNING: could not resolve dockerhub registry id; skipped proxy project" >&2
fi

# -------------------------------------------------------------- robot for CI
log "Robot account robot\$ci (pull/push on trailbase projects)"
ROBOT_BODY='{
  "name": "ci",
  "duration": -1,
  "level": "system",
  "permissions": [
    { "kind": "project", "namespace": "trailbase-sandbox",
      "access": [ {"resource":"repository","action":"pull"},
                  {"resource":"repository","action":"push"} ] },
    { "kind": "project", "namespace": "trailbase-prod",
      "access": [ {"resource":"repository","action":"pull"},
                  {"resource":"repository","action":"push"} ] }
  ]
}'
ROBOT_CODE="$(curl -s -o /tmp/harbor_robot -w '%{http_code}' "${AUTH[@]}" \
  -X POST -H 'Content-Type: application/json' -d "${ROBOT_BODY}" "${API}/robots")"
if [[ "${ROBOT_CODE}" == "201" ]]; then
  echo "    Robot created. SAVE THIS SECRET (shown once):"
  jq -r '"      name=\(.name)\n      secret=\(.secret)"' /tmp/harbor_robot
elif [[ "${ROBOT_CODE}" == "409" ]]; then
  echo "    (robot\$ci already exists; delete it in the UI to regenerate a secret)"
else
  echo "    robot creation -> HTTP ${ROBOT_CODE}: $(cat /tmp/harbor_robot)" >&2
fi

# ------------------------------------------------------------ tag retention
# Keep the 10 most recent tags per repository on the trailbase projects, so
# ~100 MB trail images don't fill the disk.
for proj in trailbase-sandbox trailbase-prod; do
  PID="$(project_id "${proj}")"
  [[ -z "${PID}" ]] && continue
  log "Tag retention on ${proj}: keep last 10"
  api_post "/retentions" "{
    \"algorithm\": \"or\",
    \"rules\": [ {
      \"disabled\": false,
      \"action\": \"retain\",
      \"template\": \"latestPushedK\",
      \"params\": { \"latestPushedK\": 10 },
      \"scope_selectors\": { \"repository\": [ {\"kind\":\"doublestar\",\"decoration\":\"repoMatches\",\"pattern\":\"**\"} ] },
      \"tag_selectors\": [ {\"kind\":\"doublestar\",\"decoration\":\"matches\",\"pattern\":\"**\"} ]
    } ],
    \"scope\": { \"level\": \"project\", \"ref\": ${PID} },
    \"trigger\": { \"kind\": \"Schedule\", \"settings\": { \"cron\": \"0 0 3 * * *\" } }
  }"
done

# ----------------------------------------------------------------- verify
log "Verifying: push a tiny image to trailbase-sandbox and wait for a scan"
HOST="${HARBOR_URL#http://}"; HOST="${HOST#https://}"
if docker pull busybox:latest >/dev/null 2>&1 \
   && docker tag busybox:latest "${HOST}/trailbase-sandbox/smoke:1" \
   && echo "${HARBOR_ADMIN_PASSWORD}" | docker login "${HOST}" -u "${HARBOR_ADMIN_USER}" --password-stdin >/dev/null 2>&1 \
   && docker push "${HOST}/trailbase-sandbox/smoke:1" >/dev/null 2>&1; then
  echo "    pushed ${HOST}/trailbase-sandbox/smoke:1; polling scan status…"
  for i in $(seq 1 30); do
    STATE="$(curl -fsS "${AUTH[@]}" \
      "${API}/projects/trailbase-sandbox/repositories/smoke/artifacts?with_scan_overview=true" \
      | jq -r '.[0].scan_overview[]?.scan_status // empty' | head -1)"
    [[ "${STATE}" == "Success" ]] && { echo "    scan: Success ✓"; break; }
    sleep 4
  done
else
  echo "    (skipped live push verification — docker not available or login failed)"
fi

log "Stage 2 complete."
