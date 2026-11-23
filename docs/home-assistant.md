# Home Assistant

There are two ways in, and you can use both:

- **DLNA player:** a `media_player` entity. Home Assistant and Music Assistant play to it.
- **MQTT:** buttons, sliders and sensors for everything else: pairing, outputs, the phone.

## DLNA player

Home Assistant discovers the speaker by itself, a few minutes after both are running.
Add it in **Settings > Devices & services > DLNA Digital Media Renderer**.
Discovery needs the app on the host network (`network_mode: host`).

The media player has play, pause, stop and volume. Use it for:

- **Text to speech:** `tts.speak` with the speaker as the media player.
- **The media browser:** local media, radio stations.
- **Music Assistant:** the speaker appears as a DLNA player.

When Home Assistant starts playing, a phone that was playing is paused. After Home Assistant
stops, the phone resumes (setting `resume_bluetooth`).

The DLNA volume is the main volume, shared with the phone.

## MQTT controls

The speaker creates its Home Assistant entities itself, through MQTT discovery. You need an
MQTT broker and the MQTT integration in Home Assistant; nothing else to configure there.

### 1. MQTT broker

**Home Assistant OS or Supervised:** install the **Mosquitto broker** add-on
(**Settings > Add-ons > Add-on store**) and start it.
Then create a user for the speaker: **Settings > People > Users > Add user**, for example
`speaker`, with a password, **Can only log in from the local network** on. The Mosquitto
add-on accepts Home Assistant users.

**Home Assistant in Docker:** run Mosquitto next to it:

```yaml
services:
  mosquitto:
    image: eclipse-mosquitto:2
    container_name: mosquitto
    restart: unless-stopped
    ports:
      - 1883:1883
    volumes:
      - ./mosquitto/config:/mosquitto/config
      - ./mosquitto/data:/mosquitto/data
```

with `./mosquitto/config/mosquitto.conf`:

```text
listener 1883
allow_anonymous false
password_file /mosquitto/config/passwords
persistence true
persistence_location /mosquitto/data/
```

Create the login: `docker exec -it mosquitto mosquitto_passwd -c /mosquitto/config/passwords speaker`,
then `docker restart mosquitto`.

### 2. MQTT integration in Home Assistant

**Settings > Devices & services > Add integration > MQTT.** On Home Assistant OS, the
Mosquitto add-on is found automatically: press **Submit**. Otherwise enter the broker's
address (`127.0.0.1` when Mosquitto runs on the same host), port `1883` and the login.

### 3. Connect the speaker

- **Home Assistant add-on:** nothing to do. With the Mosquitto add-on running, the speaker
  connects by itself.
- **Docker, CasaOS:** in the speaker's web UI, open **Settings > MQTT**, turn on
  **Enabled**, enter the broker host (the IP of the machine running Mosquitto), port `1883`,
  user and password, then **Save** and restart the container. Or set `MQTT_HOST`,
  `MQTT_USERNAME` and `MQTT_PASSWORD` in the compose file.

The log shows `mqtt connected to …`. In Home Assistant, **Settings > Devices & services >
MQTT > Devices** now lists a device named after the speaker.

### 4. Dashboard

A card with everything, assuming the speaker is named **Bluetooth Speaker** and has the
outputs **Audio jack** and **HDMI TV** (entity IDs follow the names; check them on the
device page):

```yaml
type: vertical-stack
cards:
  - type: media-control
    entity: media_player.bluetooth_speaker
  - type: entities
    title: Speaker
    entities:
      - entity: sensor.bluetooth_speaker_connected_phone
      - entity: sensor.bluetooth_speaker_now_playing
      - entity: number.bluetooth_speaker_volume
      - type: section
        label: Outputs
      - entity: switch.bluetooth_speaker_output_audio_jack
      - entity: number.bluetooth_speaker_volume_audio_jack
      - entity: switch.bluetooth_speaker_output_hdmi_tv
      - entity: number.bluetooth_speaker_volume_hdmi_tv
      - type: section
      - entity: button.bluetooth_speaker_pair_new_phone
      - entity: binary_sensor.bluetooth_speaker_pairing
  - type: horizontal-stack
    cards:
      - type: button
        entity: button.bluetooth_speaker_previous_track
        show_name: false
      - type: button
        entity: button.bluetooth_speaker_play
        show_name: false
      - type: button
        entity: button.bluetooth_speaker_pause
        show_name: false
      - type: button
        entity: button.bluetooth_speaker_next_track
        show_name: false
```

The `media-control` card needs the DLNA player (see above).

### If the entities are unavailable

- **Unavailable** means the speaker is offline for MQTT: check its log for `mqtt:` errors
  (wrong host, login or port).
- **No device at all:** the discovery prefix must match Home Assistant's (default
  `homeassistant`).
- **Renamed outputs:** new entities appear for new output names. Remove the old ones on
  the device page.

## MQTT entities

All entities belong to one device, named after the speaker.

| Entity | Description |
| - | - |
| `button` Pair new phone | Opens the pairing window |
| `binary_sensor` Pairing | On while the speaker is visible |
| `number` Volume | Main volume |
| `switch` Output *name* | Play on this output, one per output |
| `number` Volume *name* | Volume of this output, one per output |
| `sensor` Source | `idle`, `bluetooth` or `network` |
| `sensor` Connected phone | Name of the phone that plays, or `none` |
| `sensor` Now playing | Artist and title |
| `button` Play, Pause, Next track, Previous track | Control the phone |
| `button` Disconnect phone | Disconnects the phone that plays |
| `binary_sensor` Setup problem | On when the setup needs a fix; the `problems` attribute lists them |
| `sensor` Setup problem details | The first problem, or `none`; the web UI shows the fix |

Notify when the speaker needs attention:

```yaml
automation:
  - alias: Speaker needs setup
    triggers:
      - trigger: state
        entity_id: binary_sensor.bluetooth_speaker_setup_problem
        to: "on"
        for: "00:02:00"
    actions:
      - action: persistent_notification.create
        data:
          title: Bluetooth speaker
          message: "{{ states('sensor.bluetooth_speaker_setup_problem_details') }}. Open the speaker's web UI for the fix."
```

## Examples

Announcement on the speaker, then the music continues:

```yaml
action: tts.speak
target:
  entity_id: tts.google_translate_en_com
data:
  media_player_entity_id: media_player.bluetooth_speaker
  message: The washing machine is done.
```

Play on the living-room TV in the evening, on the jack during the day:

```yaml
triggers:
  - trigger: time
    at: "19:00:00"
actions:
  - action: switch.turn_on
    target:
      entity_id: switch.bluetooth_speaker_output_hdmi_tv
  - action: switch.turn_off
    target:
      entity_id: switch.bluetooth_speaker_output_audio_jack
```

## Home Assistant add-on

On Home Assistant OS, add this repository in **Settings > Add-ons > Add-on store > ⋮ >
Repositories**: `https://github.com/rosenkolev/bluetooth-to-speaker`. The web UI opens from
the sidebar (**Speaker**).
