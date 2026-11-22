# Bluetooth to Speaker

Turns the Home Assistant host into a Bluetooth speaker. Phones play to it over Bluetooth;
Home Assistant and Music Assistant play to it as a DLNA media player.

## Before you start

- The host needs a Bluetooth adapter and a sound output (3.5 mm jack, HDMI or a USB sound card).
- No other program on the host may receive Bluetooth audio.

## Use

1. Start the add-on and open **Speaker** in the sidebar.
2. Press **Pair new phone**. The speaker is visible for 2 minutes. Pick it in your phone's Bluetooth settings.
3. Choose the outputs under **Play on**, and their default volumes under **Settings**.

## Home Assistant

- **DLNA player:** Home Assistant finds it automatically (**Settings > Devices & services**,
  **DLNA Digital Media Renderer**). Use it for TTS, the media browser and Music Assistant.
- **MQTT entities:** with the Mosquitto add-on installed, the add-on connects by itself.
  You get a Pair button, volume sliders per output, output switches, the connected phone
  and what is playing.

## Settings

All settings are in the add-on's web page. They are kept in the add-on's data folder.

More: [README](https://github.com/rosenkolev/bluetooth-to-speaker#readme).
