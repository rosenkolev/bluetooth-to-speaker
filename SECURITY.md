# Security

## Reporting a vulnerability

Please report security problems privately through
[GitHub security advisories](https://github.com/rosenkolev/bluetooth-to-speaker/security/advisories/new),
not in a public issue. You get an answer within a week.

## How the app limits risk

- **Pairing:** the speaker is hidden and refuses pairing, except for a short window after
  someone presses **Pair new phone**. That window accepts one new device.
- **Container rights:** no privileged mode. The compose file drops every Linux capability
  except `SYS_NICE` (audio thread priority) and uses a read-only file system; only `/data`
  is writable.
- **Host access:** the host's D-Bus socket (to talk to BlueZ) and the sound devices.
  Access to the system D-Bus is powerful: run only images you trust, from
  `ghcr.io/rosenkolev/bluetooth-to-speaker`. Release images carry a signed build
  provenance attestation (`gh attestation verify`).
- **Network:** the web UI and the DLNA player have no login. They are meant for your home
  network; do not expose port 8780 to the internet.
- **Other web sites:** the API refuses changes requested by other web sites through your
  browser (cross-site request forgery), so a page you visit cannot open pairing or remove phones.
- **Settings:** sound output names are restricted to plain ALSA device and mixer names;
  ALSA configuration strings, which could run programs, are refused. The settings file,
  which holds the MQTT password, is readable only by its owner.
- **Discovery:** SSDP answers only senders on the local network.
