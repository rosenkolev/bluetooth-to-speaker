# Hardware

## Tested

| Board | Bluetooth | Outputs | Result |
| - | - | - | - |
| Radxa ROCK 5B+ | RTL8852BE (USB, `btusb`) | ES8316 jack (hardware volume `DAC`), HDMI 0–2 | DLNA playback, hardware volume, BlueZ registration tested. Phone streaming: to be confirmed |

Please add your board in a pull request or an issue.

## Requirements

- Linux with BlueZ 5.50 or newer (`bluetoothd`), which almost every distribution has.
- A Bluetooth adapter that supports classic Bluetooth (BR/EDR). Bluetooth LE-only adapters do not work.
- A sound output: jack, HDMI or a USB sound card.
- `arm64` or `amd64`.

## Outputs

- **Jack (3.5 mm):** usually has a hardware mixer. Choose **Hardware** as the volume control
  for the best quality at low volume.
- **HDMI:** plays only while a TV or receiver is connected and on. No hardware volume;
  set the volume on the TV or receiver, or use software volume. Many TVs add 50–150 ms
  of delay: give the other outputs that delay to stay in sync.
- **USB sound cards:** often have a hardware mixer (`PCM` or `Speaker`).
- **HDMI ARC is not supported.** ARC carries sound from the TV to a soundbar; board HDMI
  ports cannot receive it.

### Protecting the speakers

Switching a sound card on and off pops on many boards, and a card that runs out of data
clicks. The app avoids both:

- Between songs, and while paused, outputs stay open playing silence for 10 seconds, so
  the next song starts without a pop. Then they close and the amplifier can power down.
- When the data stops for a moment (Wi-Fi, a phone between songs), the output plays
  silence instead of running dry.
- Sound fades in when it starts, and fades out (30 ms) when another source takes over or
  playback stops.
- **Max volume** in the settings caps every source, including phones and Home Assistant.

## Wi-Fi and Bluetooth on one chip

Most boards have one combo chip for Wi-Fi and Bluetooth with one antenna. Heavy Bluetooth
use can slow Wi-Fi down, or freeze some chips. The app keeps radio use low: it never
searches for devices, is visible only while pairing, and offers phones a lower bitrate
(`max_bitpool` 35).

What helps most, in order:

1. **Wired network**, or **Wi-Fi on 5 GHz** (Bluetooth uses only 2.4 GHz).
2. **Wi-Fi power saving off:** `iw dev wlan0 set power_save off`.
3. **Home Assistant's Bluetooth integration on passive scanning**, or on another adapter.
4. **A USB Bluetooth dongle** on a USB 2.0 port, away from USB 3 ports (they leak 2.4 GHz
   noise), and `adapter: hci1`. Separate radios: no conflict at all.
5. **Up-to-date kernel and firmware.** Many coexistence freezes are fixed driver bugs.
