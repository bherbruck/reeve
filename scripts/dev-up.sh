#!/usr/bin/env bash
# scripts/dev-up.sh — stand up a local dev fleet: one reeve-server +
# N virtual devices, admin created, remote terminal enabled fleet-wide,
# devices enrolled. Then open http://localhost:8420.
#
#   ./scripts/dev-up.sh [N]      # N devices, default 3
#
# Re-runnable: safe to run again (setup is skipped once the admin
# exists). `./scripts/dev-down.sh` tears everything down.
set -euo pipefail

N="${1:-3}"
BASE="http://localhost:8420"
# One source of truth: export these so compose.dev.yml interpolates the
# SAME values the server seeds and we log in with. Override by setting
# REEVE_ADMIN_USER / REEVE_ADMIN_PASSWORD before running.
export REEVE_ADMIN_USER="${REEVE_ADMIN_USER:-admin}"
export REEVE_ADMIN_PASSWORD="${REEVE_ADMIN_PASSWORD:-password}"
ADMIN_USER="$REEVE_ADMIN_USER"
ADMIN_PASS="$REEVE_ADMIN_PASSWORD"
COMPOSE=(docker compose -f compose.dev.yml)
COOKIES="$(mktemp)"
trap 'rm -f "$COOKIES"' EXIT

cd "$(dirname "$0")/.."

say() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }

say "building + starting reeve-server"
"${COMPOSE[@]}" up -d --build reeve-server

say "waiting for the server to be healthy"
for _ in $(seq 1 60); do
  if curl -fsS "$BASE/healthz" >/dev/null 2>&1; then break; fi
  sleep 1
done
curl -fsS "$BASE/healthz" >/dev/null

# --- admin (password mode first-boot: setup token is logged once) ----
login() {
  curl -fsS -X POST "$BASE/api/auth/login" \
    -H 'content-type: application/json' \
    -d "{\"username\":\"$ADMIN_USER\",\"password\":\"$ADMIN_PASS\"}" \
    -c "$COOKIES" >/dev/null 2>&1
}
if login; then
  say "admin already exists — logged in"
else
  say "creating the admin user ($ADMIN_USER / $ADMIN_PASS)"
  SETUP_TOKEN="$("${COMPOSE[@]}" logs reeve-server 2>&1 \
    | grep -oE 'rvs_[a-f0-9]+' | tail -1)"
  if [ -n "$SETUP_TOKEN" ]; then
    # Don't -f: a 409 (admin already exists) is not fatal here — we
    # fall through and let the login below be the real gate.
    code="$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/auth/setup" \
      -H 'content-type: application/json' \
      -d "{\"setup_token\":\"$SETUP_TOKEN\",\"username\":\"$ADMIN_USER\",\"password\":\"$ADMIN_PASS\"}")"
    say "setup returned HTTP $code"
  fi
  if ! login; then
    echo >&2
    echo "Could not log in as $ADMIN_USER after setup." >&2
    echo "The server volume likely has a stale admin from an earlier run with" >&2
    echo "different credentials. Reset it and try again:" >&2
    echo "    just dev-down && just dev-up ${N}" >&2
    exit 1
  fi
fi

# --- enable the remote terminal fleet-wide ---------------------------
# Author config/terminal.yaml into the base layer (`00-all`, REV-010
# §11.1 — the config every device inherits); render places it in every
# device's bundle, and the agent + server both honour it. (`00-all` is
# the only layer in EVERY device's chain, so this reaches the whole
# fleet regardless of fleet/site/type assignment.)
say "enabling the remote terminal fleet-wide"
TERMINAL_CFG_B64="$(printf 'enabled: true\nidleTimeoutSecs: 600\nhardCapSecs: 3600\n' | base64 | tr -d '\n')"
curl -fsS -X PUT "$BASE/api/tree/layers/00-all" \
  -H 'content-type: application/json' -b "$COOKIES" \
  -d "{\"message\":\"dev: enable remote terminal fleet-wide\",\"files\":{\"config/terminal.yaml\":\"$TERMINAL_CFG_B64\"}}" \
  >/dev/null

# --- vendor a tiny compose stack (REV-010 §11.4 deploy target) -------
# A single-service nginx "hello" package, uploaded through the packages
# API exactly like an operator would. Idempotent: the same bytes vendor
# to the same content, so a re-run is a no-op change.
say "vendoring the demo 'hello' compose package"
HELLO_MARGO="$(cat <<'YAML'
apiVersion: margo.org/v1-alpha1
kind: ApplicationDescription
metadata:
  id: hello
  name: Hello
  version: 1.0.0
  catalog:
    organization:
      - name: Reeve Dev
        site: https://github.com/bherbruck/reeve
deploymentProfiles:
  - type: compose
    id: hello-compose
    components:
      - name: hello
        properties:
          packageLocation: ./compose.yml
YAML
)"
HELLO_COMPOSE="$(cat <<'YAML'
services:
  hello:
    image: ${REEVE_REGISTRY}/nginxdemos/hello:latest
    ports:
      - "8080:80"
YAML
)"
HELLO_MARGO_B64="$(printf '%s' "$HELLO_MARGO" | base64 | tr -d '\n')"
HELLO_COMPOSE_B64="$(printf '%s' "$HELLO_COMPOSE" | base64 | tr -d '\n')"
curl -fsS -X PUT "$BASE/api/tree/packages/hello/1.0.0" \
  -H 'content-type: application/json' -b "$COOKIES" \
  -d "{\"message\":\"dev: vendor hello demo package\",\"files\":{\"margo.yaml\":\"$HELLO_MARGO_B64\",\"compose.yml\":\"$HELLO_COMPOSE_B64\"}}" \
  >/dev/null

# --- mint a multi-use join token -------------------------------------
say "minting a join token for $N devices"
JOIN_RESP="$(curl -fsS -X POST "$BASE/api/join-tokens" \
  -H 'content-type: application/json' -b "$COOKIES" \
  -d "{\"max_uses\":$N,\"ttl_secs\":86400}")"
JOIN_TOKEN="$(printf '%s' "$JOIN_RESP" \
  | python3 -c 'import sys,json; print(json.load(sys.stdin)["join_token"])')"

# --- start the devices -----------------------------------------------
say "starting $N virtual devices"
JOIN_TOKEN="$JOIN_TOKEN" "${COMPOSE[@]}" up -d --build --scale device="$N" device

# --- populate the hierarchy (REV-010 §11.1/§11.3) --------------------
# Wait for the devices to enroll and appear, then assign them into a
# couple of fleets / sites / types (round-robin over a fixed set of
# profiles) with display names + tags, so the UI lands on a real
# populated Fleet -> Site -> Type -> Device tree, not empties.
# Idempotent: PATCH is a plain update, safe to re-run.
say "waiting for all $N devices to enroll"
device_ids() {
  curl -fsS -b "$COOKIES" "$BASE/api/devices" 2>/dev/null \
    | python3 -c 'import sys,json; [print(d["deviceId"]) for d in sorted(json.load(sys.stdin), key=lambda d: d["deviceId"])]' 2>/dev/null
}
IDS=""
for _ in $(seq 1 90); do
  IDS="$(device_ids)"
  COUNT="$(printf '%s\n' "$IDS" | grep -c . || true)"
  [ "${COUNT:-0}" -ge "$N" ] && break
  sleep 1
done
COUNT="$(printf '%s\n' "$IDS" | grep -c . || true)"
if [ "${COUNT:-0}" -lt "$N" ]; then
  say "only $COUNT/$N devices enrolled so far — assigning the ones present"
fi

# Assignment profiles: fleet | site | type | tags. Round-robin.
PROFILE_FLEET=(north  north   south)
PROFILE_SITE=(plant-a plant-a plant-b)
PROFILE_TYPE=(hmi     sensor  hmi)
PROFILE_TAGS=('{"env":"prod","line":"1"}' '{"env":"prod","line":"1"}' '{"env":"staging","line":"2"}')
NPROFILE=${#PROFILE_FLEET[@]}

say "assigning devices into fleets / sites / types"
i=0
while IFS= read -r DID; do
  [ -n "$DID" ] || continue
  p=$(( i % NPROFILE ))
  FLEET="${PROFILE_FLEET[$p]}"; SITE="${PROFILE_SITE[$p]}"
  TYPE="${PROFILE_TYPE[$p]}";   TAGS="${PROFILE_TAGS[$p]}"
  NAME="$(printf '%s %s %d' "$SITE" "$TYPE" "$((i + 1))")"
  curl -fsS -X PATCH "$BASE/api/devices/$DID" \
    -H 'content-type: application/json' -b "$COOKIES" \
    -d "{\"displayName\":\"$NAME\",\"fleet\":\"$FLEET\",\"site\":\"$SITE\",\"type\":\"$TYPE\",\"tags\":$TAGS}" \
    >/dev/null
  i=$(( i + 1 ))
done <<EOF_IDS
$IDS
EOF_IDS

# --- deploy the demo stack to a Site (REV-010 §11.4) -----------------
# Deploy `hello` to Site plant-a — reaches every device assigned to
# that site (the two `north` boxes above). The site's device manifests
# pick it up on the next render; the UI shows it under those devices.
say "deploying hello -> Site plant-a"
curl -fsS -X POST "$BASE/api/deploy" \
  -H 'content-type: application/json' -b "$COOKIES" \
  -d '{"stack":{"package":"hello","version":"1.0.0"},"scope":{"kind":"site","name":"plant-a"}}' \
  >/dev/null

cat <<EOF

  Fleet is up.

    UI:        $BASE
    login:     $ADMIN_USER / $ADMIN_PASS
    devices:   $N (Devices page — presence turns green as they connect)
    hierarchy: Fleet north -> Site plant-a -> Type hmi/sensor
               Fleet south -> Site plant-b -> Type hmi
               (open Fleet to drill in; devices renamed + tagged)
    deployed:  hello -> Site plant-a (its devices carry the stack)
    terminal:  open a device → Terminal tab → you get a shell in it

    logs:      docker compose -f compose.dev.yml logs -f
    teardown:  ./scripts/dev-down.sh
EOF
