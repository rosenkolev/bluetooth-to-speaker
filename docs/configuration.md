# Configuration

Change settings in the web UI (`http://<host>:8780`). They are saved in `/data/config.json`.
Environment variables override them at start-up.

## Settings

| Setting | Default | Description |
| - | - | - |
| `name` | `Bluetooth Speaker` | Name that phones show |
| `adapter` | `hci0` | Bluetooth adapter. Use `hci1` for a USB dongle next to a built-in adapter |
| `outputs` | `default` | Sound outputs, see below. All enabled outputs play at the same time |
| `volume` | `50` | Main volume, 0–100. Phones, DLNA, the web UI and MQTT change it |
| `max_volume` | `100` | Highest main volume any source can set |
| `latency_ms` | `200` | Buffer in milliseconds. Larger survives Wi-Fi and radio gaps better, smaller reacts faster |
| `max_bitpool` | `35` | Highest SBC quality offered to phones: 35 = Wi-Fi friendly, 53 = best quality |
| `pairing_window_secs` | `120` | How long the speaker stays visible after **Pair new phone** |
| `multipoint` | `true` | Two phones connected at once; the one that plays last wins. Off: a new phone replaces the old one |
| `reconnect_on_start` | `true` | Reconnect the last phone after a restart |
| `resume_bluetooth` | `true` | Resume the phone after Home Assistant or DLNA playback ends |
| `restore_default_volumes` | `true` | Reset output volumes to their defaults when playback starts after 5 minutes of silence |
| `http_port` | `8780` | Port of the web UI, the API and the DLNA player |
| `dlna.enabled` | `true` | DLNA player for Home Assistant and Music Assistant |
| `mqtt.enabled` | `false` | Home Assistant entities over MQTT |
| `mqtt.host`, `mqtt.port` | `localhost`, `1883` | MQTT broker |
| `mqtt.username`, `mqtt.password` | empty | MQTT login |
| `mqtt.discovery_prefix` | `homeassistant` | Home Assistant discovery prefix |

### Outputs

| Field | Description |
| - | - |
| `device` | ALSA device, for example `plughw:CARD=rockchipes8316,DEV=0`. The web UI lists the devices it finds |
| `name` | Name in the web UI and in Home Assistant |
| `enabled` | Play on this output |
| `volume` | Current volume of this output, added to the main volume |
| `default_volume` | Volume at start-up (and after a long pause, see `restore_default_volumes`) |
| `delay_ms` | Extra delay, to line up with a slower output such as a TV |
| `mixer` | Empty = software volume; `auto` or a control name (such as `DAC`) = the card's hardware mixer |

## Environment variables

| Variable | Description |
| - | - |
| `BT_NAME` | Bluetooth name |
| `BT_ADAPTER` | Bluetooth adapter |
| `BT_OUTPUT` | Outputs, separated by `;` (device names contain commas) |
| `HTTP_PORT` | Web UI port |
| `MQTT_HOST` | MQTT broker; setting it turns MQTT on |
| `MQTT_PORT`, `MQTT_USERNAME`, `MQTT_PASSWORD` | MQTT settings |
| `ADVERTISE_IP` | IP address announced to DLNA controllers, when the automatic choice is wrong |
| `DATA_DIR` | Settings folder (default `/data`) |
| `RUST_LOG` | Log level: `error`, `warn`, `info` (default) or `debug` |
