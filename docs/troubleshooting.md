# Troubleshooting

The app checks its setup at start and every minute. When something is wrong, the web UI
shows a **Setup** card with the cause and the command that fixes it, the log has the same
line, and Home Assistant (MQTT) has a **Setup problem** sensor. After a fix, press **Check
again**. The app never changes the host itself.

The log: `docker logs bluetooth-to-speaker` (add-on: the **Log** tab).

## Runs as root

The app starts as root only to give the data folder and the sound devices to user 1000,
then runs as that user. Through the host's D-Bus, root could control the whole host; a
normal user can use Bluetooth and nothing else. When that switch is not possible, the app
stays root (everything still works) and says why:

- **Missing capabilities**: add `CHOWN`, `SETUID` and `SETGID` to `cap_add` (see
  [compose.yaml](../deploy/compose.yaml)).
- **BlueZ allows only root**: some distributions limit BlueZ to root and the `bluetooth`
  group. Allow user 1000, then restart the app:

  ```sh
  echo '<busconfig><policy user="1000"><allow send_destination="org.bluez"/></policy></busconfig>' \
    | sudo tee /etc/dbus-1/system.d/bluetooth-to-speaker.conf
  sudo systemctl reload dbus
  ```

- Another user: set `PUID` and `PGID`. To stay root on purpose: `RUN_AS_ROOT=1`.

Going back to version 0.1.0 after a newer version: 0.1.0 runs as root and cannot read the
settings files of the new user, and replaces them with defaults. Give the files to root
first: `sudo chown -R 0:0 <data folder>`.

## The phone does not see the speaker

The speaker is hidden on purpose. Press **Pair new phone** in the web UI (or the Home
Assistant button); it is visible for 2 minutes.

## Another program takes the phone's audio

Only one program can be the Bluetooth speaker. The app finds bluez-alsa as a service, and
PipeWire or PulseAudio when a phone's sound goes to them instead. Look for them:

```sh
ps -e | grep -Ei 'pipewire|pulse|wireplumber|bluealsa'
```

- **bluez-alsa**: `sudo systemctl disable --now bluealsa bluealsa-aplay`
- **PipeWire** (desktops): keep Bluetooth headsets working, and stop PipeWire from being a
  speaker:

  ```sh
  mkdir -p ~/.config/wireplumber/wireplumber.conf.d
  echo 'monitor.bluez.properties = { bluez5.roles = [ a2dp_source hfp_ag hsp_ag bap_source ] }' \
    > ~/.config/wireplumber/wireplumber.conf.d/90-no-speaker.conf
  systemctl --user restart wireplumber
  ```

- **PulseAudio**: remove its Bluetooth module (`sudo apt remove pulseaudio-module-bluetooth`).

## "BlueZ not reachable" or "No adapter"

- `/run/dbus` must be mounted into the container, and `bluetoothd` must run on the host
  (`systemctl status bluetooth`). Not installed: `sudo apt install bluez` (Debian, Ubuntu,
  Raspberry Pi OS), `sudo dnf install bluez` (Fedora), then
  `sudo systemctl enable --now bluetooth`.
- The adapter must be on: `bluetoothctl power on`. Blocked: `sudo rfkill unblock bluetooth`.
  Check the name with `bluetoothctl list` and set `adapter` (`BT_ADAPTER`) to match.

## No sound

- Check **Play on** in the web UI: at least one output must be on, and its state should be
  **playing** while music plays.
- An output shows **unavailable**: the device is missing. For HDMI, the TV must be on.
- **Used by another program**: PulseAudio, PipeWire or another player on the host keeps the
  card open. `sudo fuser -v /dev/snd/*` shows which.
- Jack still silent: some codecs start muted. Run `alsamixer -c <card>` on the host and
  unmute the headphone or line-out switches.
- `/dev/snd` must be passed to the container.

## Settings are lost after a restart

The data folder is not writable. The app gives it to its user at start (with the `CHOWN`
capability); without that capability, give it yourself: `sudo chown -R 1000:1000 ./data`.

## The web UI does not open from other devices

It opens on the host itself (`curl http://127.0.0.1:8780`) but not from your PC or phone:
the host's firewall blocks it. The app uses the host network, so the firewall filters it,
unlike containers with published ports, which Docker opens itself. Open TCP 8780 (web UI,
DLNA player) and UDP 1900 (DLNA discovery):

```sh
# firewalld (Fedora, Rocky, Debian with firewalld): zone of your LAN interface
sudo firewall-cmd --permanent --zone=home --add-port=8780/tcp --add-port=1900/udp
sudo firewall-cmd --reload
# ufw (Ubuntu)
sudo ufw allow 8780/tcp && sudo ufw allow 1900/udp
```

The app reads firewalld's rules and shows the exact command for your zone. It cannot read
ufw's.

## Home Assistant entities are missing

MQTT is off, or the broker refuses the app. Set the broker in **Settings > MQTT** in the
web UI (the add-on finds the Mosquitto add-on by itself). See
[Home Assistant](home-assistant.md).

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
