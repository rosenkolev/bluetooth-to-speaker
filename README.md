# Bluetooth to Speaker

<p align="center"><img src="docs/images/icon-256.png" alt="" width="96"></p>

[![CI](https://github.com/rosenkolev/bluetooth-to-speaker/actions/workflows/ci.yml/badge.svg)](https://github.com/rosenkolev/bluetooth-to-speaker/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/rosenkolev/bluetooth-to-speaker)](https://github.com/rosenkolev/bluetooth-to-speaker/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

Turn a Raspberry Pi, a Radxa board, a NAS or any Linux box with Bluetooth into a Bluetooth
speaker. Phones play to it like to any speaker. Home Assistant and Music Assistant play to it
too. It runs as one small container: about 6 MB of memory.

## Features

- **Bluetooth speaker.** Phones stream over Bluetooth (A2DP, SBC). The phone's volume buttons
  and the speaker volume stay in sync.
- **Safe pairing.** The speaker stays hidden until you press **Pair new phone**; then it is
  visible for 2 minutes. Paired phones reconnect at any time.
- **Switch phones.** Pick the speaker on another paired phone or tablet and press play.
- **Several outputs.** Jack, HDMI and USB sound cards, alone or several at once, kept in sync.
  Each output has its own volume, default volume and delay.
- **Home Assistant.** A DLNA media player for TTS, the media browser and Music Assistant,
  plus MQTT entities for pairing, outputs and volumes. When Home Assistant speaks, the phone
  pauses and resumes afterwards.
- **Web UI** for everything, on port 8780.
- **Gentle on Wi-Fi.** Low radio use, for boards where Wi-Fi and Bluetooth share one chip.
- **No host changes.** Uses the host's BlueZ over D-Bus. No privileged mode.

## Install

### Docker Compose

```yaml
services:
  bluetooth-to-speaker:
    image: ghcr.io/rosenkolev/bluetooth-to-speaker:latest
    container_name: bluetooth-to-speaker
    restart: unless-stopped
    network_mode: host
    devices:
      - /dev/snd:/dev/snd
    volumes:
      - /run/dbus:/run/dbus:ro
      - ./data:/data
    cap_drop: [ALL]
    cap_add: [SYS_NICE]
    read_only: true
```

Then open `http://<host>:8780` and press **Pair new phone**. With a host firewall
(firewalld, ufw), open TCP 8780 and UDP 1900 first: see [troubleshooting](docs/troubleshooting.md#the-web-ui-does-not-open-from-other-devices).
A commented version is in [deploy/compose.yaml](deploy/compose.yaml).

### Home Assistant add-on

[![Add repository](https://my.home-assistant.io/badges/supervisor_add_addon_repository.svg)](https://my.home-assistant.io/redirect/supervisor_add_addon_repository/?repository_url=https%3A%2F%2Fgithub.com%2Frosenkolev%2Fbluetooth-to-speaker)

Or add `https://github.com/rosenkolev/bluetooth-to-speaker` in
**Settings > Add-ons > Add-on store > ⋮ > Repositories**, then install **Bluetooth to Speaker**.

### CasaOS, Runtipi, Docker Apps

Coming in the app store [rosenkolev/app-store](https://github.com/rosenkolev/app-store).

## Before you start

- **BlueZ** must run on the host (`bluetoothd`); almost every Linux has it.
- **No other Bluetooth audio receiver** on the host: PipeWire or PulseAudio with Bluetooth,
  or bluez-alsa, compete for the phone. See [troubleshooting](docs/troubleshooting.md).
- `arm64` or `amd64`.

## Documentation

- [Configuration](docs/configuration.md): every setting and environment variable
- [Home Assistant](docs/home-assistant.md): DLNA player, MQTT entities, examples
- [Hardware](docs/hardware.md): tested boards, outputs, Wi-Fi and Bluetooth on one chip
- [Troubleshooting](docs/troubleshooting.md)
- [Development](docs/development.md): how it works, building, the test sandbox, releases

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Security reports: [SECURITY.md](SECURITY.md).

## License

[MIT](LICENSE). The image includes libsbc (LGPL-2.1, linked dynamically) and alsa-lib
(LGPL-2.1); Rust dependencies are under MIT, Apache-2.0, MPL-2.0 and similar licenses.
