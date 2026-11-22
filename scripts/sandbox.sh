#!/usr/bin/env bash
# Test sandbox on a Docker host: Bluetooth to Speaker (built from this checkout), Home
# Assistant and Mosquitto inside Docker-in-Docker, isolated from the host's containers.
# The app uses the host's Bluetooth service (D-Bus) and sound cards.
#
#   scripts/sandbox.sh up       create or start the sandbox, build and start the app, set up Home Assistant
#   scripts/sandbox.sh deploy   copy the source, rebuild the image and restart the app
#   scripts/sandbox.sh logs [N] last N log lines of the app (default 100)
#   scripts/sandbox.sh status   app state from its API
#   scripts/sandbox.sh tunnel   forward http://localhost:18780 (app) and :18124 (Home Assistant)
#   scripts/sandbox.sh token    print an access token of the Home Assistant test account
#   scripts/sandbox.sh sh CMD   run a shell command in the sandbox Docker engine
#   scripts/sandbox.sh down     remove the sandbox with all its containers, images and data
#
# Settings and the generated test account are kept in .sandbox.env (git ignored):
#   SANDBOX_SSH       ssh target of the Docker host, for example user@192.168.0.10 (required)
#   SANDBOX_SSH_KEY   ssh key (optional)
#   SANDBOX_DOCKER    docker command on the host (default: sudo -n docker)
#   SANDBOX_DIR       folder on the host (default: bluetooth-to-speaker-sandbox in the home folder)
#   SANDBOX_HA        Home Assistant image (default: ghcr.io/home-assistant/home-assistant:stable)
# cspell:words dind certdir urandom
# Commands in single quotes run on the Docker host, where $S is set.
# shellcheck disable=SC2016
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
ENV_FILE="$ROOT/.sandbox.env"
# shellcheck source=/dev/null
[[ -f $ENV_FILE ]] && . "$ENV_FILE"
: "${SANDBOX_SSH:?set SANDBOX_SSH in $ENV_FILE, for example SANDBOX_SSH=user@192.168.0.10}"
SANDBOX_DOCKER=${SANDBOX_DOCKER:-sudo -n docker}
SANDBOX_DIR=${SANDBOX_DIR:-bluetooth-to-speaker-sandbox}
SANDBOX_HA=${SANDBOX_HA:-ghcr.io/home-assistant/home-assistant:stable}
SANDBOX_USER=${SANDBOX_USER:-sandbox}
DIND=docker:27.5.1-dind
NAME=bt2speaker-sandbox
APP_PORT=18780
HA_PORT=18124
CLIENT=http://localhost:$HA_PORT/

SSH=(ssh -o BatchMode=yes)
[[ -n ${SANDBOX_SSH_KEY:-} ]] && SSH+=(-i "$SANDBOX_SSH_KEY" -o IdentitiesOnly=yes)
SSH+=("$SANDBOX_SSH")

remote() { "${SSH[@]}" "D='$SANDBOX_DOCKER'; S=\$HOME/$SANDBOX_DIR; $1"; }
inner() { printf %s "$1" | remote "\$D exec -i $NAME sh -s"; }

# api METHOD PATH BODY [TOKEN]: call the sandbox Home Assistant from the host
api() {
  local auth=""
  [[ -n ${4:-} ]] && auth="-H 'Authorization: Bearer $4'"
  local type=application/json
  [[ $3 == *=* && $3 != "{"* ]] && type=application/x-www-form-urlencoded
  printf %s "$3" | "${SSH[@]}" \
    "curl -sf -X $1 http://127.0.0.1:$HA_PORT$2 -H 'Content-Type: $type' $auth --data-binary @-"
}

field() { grep -o "\"$1\":\"[^\"]*\"" | head -1 | cut -d'"' -f4; }

save_env() {
  {
    echo "SANDBOX_SSH=$SANDBOX_SSH"
    [[ -n ${SANDBOX_SSH_KEY:-} ]] && echo "SANDBOX_SSH_KEY=$SANDBOX_SSH_KEY"
    echo "SANDBOX_USER=$SANDBOX_USER"
    echo "SANDBOX_PASSWORD=$SANDBOX_PASSWORD"
  } >"$ENV_FILE"
}

copy_source() {
  tar -C "$ROOT" -cf - Cargo.toml Cargo.lock build.rs rustfmt.toml Dockerfile .dockerignore src web |
    remote 'rm -rf $S/workspace/src && mkdir -p $S/workspace/src && tar -C $S/workspace/src -xf -'
}

build_and_start() {
  local version
  version="sandbox-$(date +%Y%m%d%H%M)"
  echo "building the image in the sandbox (first build takes a few minutes)"
  inner "docker build -q -t bluetooth-to-speaker:dev --build-arg APP_VERSION=$version /workspace/src >/dev/null &&
docker compose -f /workspace/compose.yaml up -d --quiet-pull"
}

wait_ha() {
  remote "for i in \$(seq 1 90); do curl -sf -o /dev/null http://127.0.0.1:$HA_PORT/manifest.json && exit 0; sleep 2; done; exit 1" ||
    { echo "Home Assistant did not start" >&2; exit 1; }
}

tokens() {
  local flow code
  flow=$(api POST /auth/login_flow "{\"client_id\":\"$CLIENT\",\"handler\":[\"homeassistant\",null],\"redirect_uri\":\"$CLIENT\"}" | field flow_id)
  code=$(api POST "/auth/login_flow/$flow" "{\"client_id\":\"$CLIENT\",\"username\":\"$SANDBOX_USER\",\"password\":\"$SANDBOX_PASSWORD\"}" | field result)
  api POST /auth/token "grant_type=authorization_code&code=$code&client_id=$CLIENT"
}

token() { tokens | field access_token; }

# discovered_flow HANDLER TOKEN: id of a discovery flow in progress (only on the websocket API),
# read with the Python and aiohttp of the Home Assistant container
discovered_flow() {
  inner "docker exec -i homeassistant python3 - $1 $2 <<'PY'
import asyncio, sys, aiohttp
async def main(handler, token):
    async with aiohttp.ClientSession() as s, s.ws_connect('http://127.0.0.1:8123/api/websocket') as ws:
        await ws.receive_json()
        await ws.send_json({'type': 'auth', 'access_token': token})
        await ws.receive_json()
        await ws.send_json({'id': 1, 'type': 'config_entries/flow/progress'})
        for flow in (await ws.receive_json()).get('result') or []:
            if flow['handler'] == handler:
                print(flow['flow_id'])
                return
asyncio.run(main(sys.argv[1], sys.argv[2]))
PY" 2>/dev/null || true
}

setup_ha() {
  wait_ha
  if api GET /api/onboarding "" | grep -q '"step":"user","done":false'; then
    SANDBOX_PASSWORD=${SANDBOX_PASSWORD:-$(head -c 18 /dev/urandom | base64 | tr -d '/+=')}
    save_env
    local code access
    code=$(api POST /api/onboarding/users "{\"client_id\":\"$CLIENT\",\"name\":\"Sandbox\",\"username\":\"$SANDBOX_USER\",\"password\":\"$SANDBOX_PASSWORD\",\"language\":\"en\"}" | field auth_code)
    access=$(api POST /auth/token "grant_type=authorization_code&code=$code&client_id=$CLIENT" | field access_token)
    api POST /api/onboarding/core_config "{}" "$access" >/dev/null
    api POST /api/onboarding/analytics "{}" "$access" >/dev/null
    api POST /api/onboarding/integration "{\"client_id\":\"$CLIENT\",\"redirect_uri\":\"${CLIENT}?auth_callback=1\"}" "$access" >/dev/null
    echo "Home Assistant onboarded, test account in $ENV_FILE"
  fi

  local access entries flow
  access=$(token)
  entries=$(api GET /api/config/config_entries/entry "" "$access")
  if ! grep -q '"domain":"mqtt"' <<<"$entries"; then
    flow=$(api POST /api/config/config_entries/flow '{"handler":"mqtt"}' "$access" | field flow_id)
    if api POST "/api/config/config_entries/flow/$flow" \
      '{"broker":"127.0.0.1","port":1883,"protocol":"5","other_settings":{"set_client_cert":false,"set_ca_cert":"off","transport":"tcp"}}' \
      "$access" | grep -q create_entry; then
      echo "MQTT set up"
    else
      echo "MQTT was not set up; add it in Home Assistant (broker 127.0.0.1)" >&2
    fi
  fi
  if ! grep -q '"domain":"dlna_dmr"' <<<"$entries"; then
    # Home Assistant discovers the DLNA player over SSDP (a few minutes after its first
    # start); the user step then lists it by name.
    local found=""
    for _ in $(seq 1 100); do
      found=$(discovered_flow dlna_dmr "$access")
      [[ -n $found ]] && break
      sleep 3
    done
    if [[ -n $found ]] && api POST "/api/config/config_entries/flow/$found" '{}' "$access" | grep -q create_entry; then
      echo "DLNA player added to Home Assistant"
    else
      echo "the DLNA player was not discovered; check Settings > Devices in Home Assistant" >&2
    fi
  fi
}

up() {
  # Only the workspace folder is created here; Docker creates the data folders as root,
  # which the app needs: it runs as root without capabilities (no DAC override).
  remote "mkdir -p \$S/workspace && cd \$S
cat > compose.yaml <<EOF
# Bluetooth to Speaker sandbox: its own Docker engine, isolated from the host containers
name: $SANDBOX_DIR
services:
  dind:
    image: $DIND
    container_name: $NAME
    privileged: true
    restart: unless-stopped
    environment:
      DOCKER_TLS_CERTDIR: ''
    # only on localhost: reach it through an SSH tunnel
    ports:
      - 127.0.0.1:$APP_PORT:8780
      - 127.0.0.1:$HA_PORT:8123
    volumes:
      - ./engine:/var/lib/docker
      - ./workspace:/workspace
      - /run/dbus:/host/run/dbus:ro
EOF
cat > workspace/compose.yaml <<EOF
name: sandbox
services:
  app:
    image: bluetooth-to-speaker:dev
    container_name: bluetooth-to-speaker
    network_mode: host
    restart: unless-stopped
    devices:
      - /dev/snd:/dev/snd
    volumes:
      - /host/run/dbus:/run/dbus:ro
      - /workspace/data:/data
    cap_drop: [ALL]
    cap_add: [SYS_NICE]
    read_only: true
    security_opt: ['no-new-privileges:true']
    environment:
      BT_NAME: Sandbox Speaker
      MQTT_HOST: 127.0.0.1
      RUST_LOG: info
  homeassistant:
    image: $SANDBOX_HA
    container_name: homeassistant
    network_mode: host
    restart: unless-stopped
    volumes:
      - /workspace/ha-config:/config
  mosquitto:
    image: eclipse-mosquitto:2
    container_name: mosquitto
    network_mode: host
    restart: unless-stopped
    command: mosquitto -c /mosquitto-no-auth.conf
EOF
\$D compose up -d --quiet-pull
for i in \$(seq 1 30); do \$D exec $NAME docker info >/dev/null 2>&1 && break; sleep 2; done"
  copy_source
  build_and_start
  setup_ha
  echo "ready: run '$0 tunnel', then open http://localhost:$APP_PORT and $CLIENT"
}

case ${1:-} in
up) up ;;
deploy)
  copy_source
  build_and_start
  sleep 2
  inner 'docker logs --tail 15 bluetooth-to-speaker 2>&1'
  ;;
logs) inner "docker logs --tail ${2:-100} bluetooth-to-speaker 2>&1" ;;
status) remote "curl -s http://127.0.0.1:$APP_PORT/api/state" ;;
token) token ;;
sh) inner "$2" ;;
tunnel)
  echo "app: http://localhost:$APP_PORT  Home Assistant: $CLIENT"
  exec "${SSH[@]}" -N -o ExitOnForwardFailure=yes \
    -L "$APP_PORT:127.0.0.1:$APP_PORT" -L "$HA_PORT:127.0.0.1:$HA_PORT"
  ;;
down)
  # the engine folder belongs to root: delete it from a container
  remote "[ -d \$S ] || exit 0
\$D exec $NAME sh -c 'docker ps -aq | xargs -r docker rm -f >/dev/null; docker system prune -af --volumes >/dev/null' 2>/dev/null || true
cd \$S && \$D compose down -v
\$D run --rm -v \$S:/s $DIND sh -c 'rm -rf /s/engine /s/workspace'
cd && rm -rf \$S"
  echo "sandbox removed"
  ;;
*)
  sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'
  exit 1
  ;;
esac
