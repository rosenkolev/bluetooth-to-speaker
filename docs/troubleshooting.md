# Troubleshooting

Start with the log: `docker logs bluetooth-to-speaker` (add-on: the **Log** tab).

## The phone does not see the speaker

The speaker is hidden on purpose. Press **Pair new phone** in the web UI (or the Home
Assistant button); it is visible for 2 minutes.

## The phone connects, but the sound comes from somewhere else, or not at all

Another program on the host receives Bluetooth audio. Look for PipeWire, PulseAudio or
bluez-alsa:

```sh
ps -e | grep -Ei 'pipewire|pulse|wireplumber|bluealsa'
```

Disable them (`systemctl --user disable --now pipewire pipewire-pulse wireplumber`,
`sudo systemctl disable --now bluealsa bluealsa-aplay`), or remove their Bluetooth modules.

## "BlueZ not reachable" or "No adapter"

- `/run/dbus` must be mounted into the container, and `bluetoothd` must run on the host
  (`systemctl status bluetooth`).
- The adapter must be on: `bluetoothctl power on`. Check the name with `bluetoothctl list`
  and set `adapter` (`BT_ADAPTER`) to match.

## No sound

- Check **Play on** in the web UI: at least one output must be on, and its state should be
  **playing** while music plays.
- An output shows **unavailable**: the device is missing. For HDMI, the TV must be on.
- Jack still silent: some codecs start muted. Run `alsamixer -c <card>` on the host and
  unmute the headphone or line-out switches.
- `/dev/snd` must be passed to the container.

## Settings are lost after a restart

The log says `cannot save settings`. The data folder must be writable by root: the
container runs as root without capabilities, so it cannot write to a folder owned by
another user. Let Docker create the folder, or `sudo chown root: ./data`.

## Home Assistant does not find the DLNA player

- The app needs the host network (`network_mode: host`) for SSDP discovery.
- Discovery can take a few minutes after Home Assistant starts.
- Several network interfaces (Docker bridges, VPN): set `ADVERTISE_IP` to the LAN address.

## Wi-Fi drops or the board freezes while streaming

See [Wi-Fi and Bluetooth on one chip](hardware.md#wi-fi-and-bluetooth-on-one-chip).

## Stutter

- Raise `latency_ms` (buffer) to 300–500.
- Lower the quality to **Wi-Fi friendly**.
- Check the **underruns** counter under **Diagnostics** in the web UI.
