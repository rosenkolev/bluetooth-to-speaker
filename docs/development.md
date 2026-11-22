# Development

## Layout

```text
src/
  main.rs      start-up: settings, tasks, signal handling
  app.rs       shared state, volumes, settings
  bluez.rs     BlueZ over D-Bus: A2DP sink endpoint, pairing agent, AVRCP, phone switching
  sbc.rs       libsbc decoder (FFI)
  player.rs    one source at a time: Bluetooth and network (DLNA) streams
  group.rs     several outputs at once: ring buffers, clock drift correction, delay, volume
  output.rs    ALSA output, hardware mixers, device list
  dlna.rs      UPnP/DLNA media renderer (SOAP)
  ssdp.rs      SSDP discovery
  mqtt.rs      MQTT with Home Assistant discovery
  web.rs       web UI and JSON API
web/index.html the web UI (one file, built into the binary)
bluetooth-to-speaker/  Home Assistant add-on
deploy/compose.yaml    plain Docker Compose
scripts/sandbox.sh     test sandbox on a board
```

## How it works

1. The app registers an A2DP sink endpoint and a pairing agent with the host's BlueZ over
   the system D-Bus. It uses only a unique bus name, so no D-Bus policy file is needed.
2. When a phone streams, BlueZ hands over a socket with the RTP/SBC packets
   (`MediaTransport1.Acquire`). A real-time thread decodes them with libsbc.
3. The decoded audio goes to an output group: one writer thread per sound card, each
   correcting its clock drift by dropping or repeating a single frame now and then.
4. DLNA playback (Home Assistant, Music Assistant) decodes HTTP streams with symphonia and
   uses the same output group. The last source to start owns the outputs.

## Build

```sh
sudo apt-get install libasound2-dev libsbc-dev   # Debian, Ubuntu
cargo build --release
docker build -t bluetooth-to-speaker .
```

## Sandbox

`scripts/sandbox.sh` tests on a real board over SSH. It runs Docker-in-Docker with the app
(built from your checkout), Home Assistant and Mosquitto, isolated from the board's own
containers. The app uses the board's BlueZ and sound cards.

```sh
cat > .sandbox.env <<EOF
SANDBOX_SSH=user@192.168.0.10
SANDBOX_SSH_KEY=~/.ssh/id_ed25519
EOF
scripts/sandbox.sh up       # first run: builds, onboards Home Assistant, adds MQTT and DLNA
scripts/sandbox.sh deploy   # after a change
scripts/sandbox.sh logs
scripts/sandbox.sh tunnel   # app on http://localhost:18780, Home Assistant on :18124
scripts/sandbox.sh down
```

The SSH user needs `sudo docker` without a password. Stop other Bluetooth audio receivers on
the board first (see [troubleshooting](troubleshooting.md)).

## API

| Method | Path | Description |
| - | - | - |
| GET | `/api/state` | Status, volumes, outputs, phones, stats |
| GET, PUT | `/api/config` | Settings |
| GET | `/api/outputs` | Sound devices found on the board, with their mixers |
| POST | `/api/outputs/enable` | `{device, enabled}` |
| POST | `/api/outputs/volume` | `{device, volume}` |
| POST | `/api/outputs/restore` | Default volumes |
| POST | `/api/volume` | `{volume}` main volume |
| POST | `/api/pair`, `/api/pair/cancel` | Pairing window |
| POST | `/api/media/{play,pause,next,previous}` | Control the phone |
| POST | `/api/devices/{address}/{connect,disconnect,remove,play}` | One phone |

## Release

1. Merge pull requests into `main`; each push publishes the `edge` image.
2. Release Drafter keeps a draft release with the notes and the next version
   (from the labels `breaking-change`, `feature`, `bug`).
3. Publish the draft. The tag builds `ghcr.io/rosenkolev/bluetooth-to-speaker:<version>`
   (and `latest`), then the workflow sets the add-on version to match.
