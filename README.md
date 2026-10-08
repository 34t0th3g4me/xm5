# xm5 – Sony WH-1000XM5 Controller for Linux

TUI + CLI for the Sony WH-1000XM5 (works with any BT headset): profile switching
music ⇄ microphone, volume, mic enhancement, repair, battery.
Tested on Omarchy (Arch + PipeWire + Hyprland); also runs on Ubuntu
(PipeWire **or** PulseAudio — both are covered via `pactl`).

## Why this is needed

In Bluetooth music mode (A2DP) there is no headset microphone. Ordinary desktops
then automatically switch to HFP as soon as any client asks for the microphone
(including WebRTC device scans in the browser!) — every switch is audible as a
disconnect glitch, and the XM5 jingles get annoying.

The fix in WirePlumber: `bluez5.autoswitch-to-headset-profile = false`
(`~/.config/wireplumber/wireplumber.conf.d/51-no-xm5-autoswitch.conf`).
Afterwards the XM5 mic runs as an **A2DP source in parallel** (16 kHz, music
quality stays untouched) — browsers/calls detect it immediately, without a
profile flip. Classic HFP mode remains available manually via the TUI (`V`)
or `xm5 mode mic`.


## Installation

```sh
git clone https://github.com/34t0th3g4me/xm5.git
cd xm5
python3 install.py
```

This installs everything: `xm5` in `~/.local/bin`, the systemd user unit and
the Hyprland shortcuts (Omarchy only). `xm5-tui` is provisioned automatically,
offline-first, and no network is required:

1. an already-built `xm5-tui/target/release/xm5-tui` is reused, else
2. a bundled **static** binary `xm5-tui/prebuilt/xm5-tui` (musl static-pie — runs
   on any glibc/musl distro, no Rust toolchain needed), else
3. `cargo build --release --offline` against the crates snapshot in
   `xm5-tui/vendor.tar.xz` (extracted to `~/.cache/xm5`), else
4. a plain networked `cargo build`, falling back to the bundled binary if that
   fails.

Without a Rust toolchain the bundled static binary is installed directly, so
the TUI works even on machines that cannot reach crates.io.

What it does:
1. `xm5-tui` → `~/.local/bin/xm5-tui-bin` (+ wrapper `xm5-tui`) — provisioned as above
2. `~/.local/bin/xm5` (+ `xm5-music`, `xm5-mic`, `xm5-toggle`, `xm5-volup`, `xm5-voldown`, `xm5-repair`)
3. systemd user unit `xm5-pilot.service` (keeps profile + default sink/source stable across reconnects, logs to `~/.config/xm5/pilot.log`)
4. Hyprland shortcuts (Omarchy only):
   - `SUPER + ALT + M` → open TUI (again: focus it)
   - `SUPER + ALT + SHIFT + UP/DOWN` → volume ±5 % (repeatable)
   - `SUPER + ALT + SHIFT + R` → repair
   - (plain `SUPER+ALT+UP/DOWN` stays reserved for Omarchy's window groups)

On Ubuntu/GNOME: just start the TUI with `xm5` in a terminal; you can add the
shortcuts under Settings › Keyboard › Custom Shortcuts (`xm5-toggle`,
`xm5-volup`, `xm5-voldown`).

## TUI (Nothing-X look, 2 columns, mouse)

```sh
xm5          # opens the TUI dashboard (xm5-tui)
xm5-tui      # directly
```

Dense two-column dashboard in the Nothing-X style (near-black, gray tiles,
rounded borders, white active pods, green status dot):

```
 ● WH-1000XM5  auto    󰂁 80%
╭ device ────────────╮╭ noise cancel ────────────────╮
│ 󰋎 SONY WH-1000XM5  ││   ╭─────╮   ╭─────╮  ╭─────╮  │
│ mode music [aac]   ││   │ 󰆩 │   │ 󰙅 │  │ 󰋐 │  │
│ 󰂯 connected · …    ││   ╰─────╯   ╰─────╯  ╰─────╯  │
╰────────────────────╯│ noise cancel  ambient   off   │
                      │  15 ━━━━━━━━━━━━━━━  54%      │
╭ volume ────────────╮╭ quick ────────────────────────╮
│ 󰕾 80%              ││ [M] music [V] mic [T] toggle  │
│  ┃┃┃┃┃┃┃┃┃┃┃┃━━    ││ [E] dsp   [R] repair    auto  │
╰────────────────────╯╰───────────────────────────────╯
╭ mic DSP ●╮ ╭ repair bluetooth ╮ ╭ ● auto pilot ╮
```

**Mouse**: click pods (ANC/ambient/off), click+drag in the volume tick bar
(direct position), mouse wheel = volume ±3, all chips clickable (including
`auto` = toggle pilot mode).

**Keys**: `1`/`2`/`3` pods · `←`/`→` pods · `↑`/`↓` ANC level (1–28) ·
`+`/`-` volume · `m/v/t` music/mic/toggle · `e` mic DSP · `r` repair ·
`F` refresh · `q` quit

**Layout**: responsive — 78×14+ two columns, narrower stacks into one column
(device · ANC · [media] · volume · quick · mic). Every tile is allocated at
its content minimum first (bordered tiles never get fewer than 3 rows, the mic
scope 6, the media tile 5); tiles that cannot reach that are dropped and the
rest re-fitted, so nothing is ever rendered clipped or blank. The mic scope
shows the incoming XM5 track live (pw-cat capture, ~8 fps, dB + last level in
accent blue); when there is no device or the mic is inactive, a hint about mic
mode appears. The footer shows the armed quick chips plus the current
mode/profile on the right — both sides are width-budgeted and clipped instead
of overlapping; all keys are in the `?` overlay.

**Design/animations**: start stagger (tile borders flash in accent blue),
pod click flash (350 ms), volume changes glow in the bar (accent → gradient),
status dot breathes (2.4 s), toast with accent pop-in + fade-out. All icons
are verified against the actually installed Nerd Font (Pango render + cmap):
󰖔 ANC (moon), 󰞍 waves (ambient), 󰋐 headset-off, 󰝚 music, 󰓡 swap, 󰘮 tuner (DSP),
󰒓 gear (repair), 󰁨 auto-fix (pilot), 󰂯 bluetooth, battery ladder. Respects
`NO_COLOR`.

**Latency**: pod and level clicks apply immediately (optimistic); the XM5
confirmation runs in the background (no UI blocking). Status refresh only on
change (the pilot service writes `~/.local/state/xm5/status.json` every 2 s;
the TUI reads that file instead of spawning processes) — idle CPU ≈ 0.2 %
instead of 1.3 %. Layout scales from 50×17 up to fullscreen.

## UI sounds

`xm5` plays short synth jingles on mode switches (`sounds/*.wav`):
`music`, `mic`, `connected`, `power_on`, `power_off`, `error`.
For ANC/volume **there are no PC sounds** — the XM5 plays its own device
jingle for those (otherwise double). Turn off with `XM5_SOUNDS=0`.

## Known issue (fixed)

The Sony WH-1000XM5 likes to die mid profile switch: the HFP SCO transport
then only delivers zeros (mic receives nothing) even though everything looks
"connected". `xm5` detects this automatically since 2026-10-05:
- `xm5 mode mic` and `xm5 repair` verify mic data after switching (`mic_alive`)
- With a dead SCO link it automatically disconnects/reconnects + resets profile
- Afterwards the browser and pw-record deliver real samples again.

If it ever hangs: just run `xm5 repair` (or TUI key `R`).

## CLI

```sh
xm5 status                 # JSON status (profile, codec, volume, battery)
xm5 mode music             # A2DP AAC (music, no mic)
xm5 mode mic               # HSP/HFP MSBC (mic active)
xm5 mode toggle            # switch
xm5 mode auto              # enable pilot service (repairs defaults only)
xm5 volume + | - | 90 | max | set 70
xm5 mic-enhance on|off|toggle|status
xm5 anc on|off             # noise cancelling on/off (directly on the XM5, RFCOMM)
xm5 anc ambient 12         # ambient sound mode, level 1–28
xm5 anc voice 10           # voice focus (ambient + voices)
xm5 anc status             # last applied state
xm5 battery
xm5 repair                 # repair broken BT audio connection
xm5 discover               # list paired BT devices
```

## Noise cancelling / ambient sound

Runs over Sony's proprietary RFCOMM protocol (channel 9, service
`956c7b26-…`) — same path as the official app:
- `A` in the TUI = ANC on, `C` = ANC/ASM fully off, `V` = voice focus
- `xm5 anc ambient <level>` = ambience with adjustable level (1–28)
- The XM5 ACKs every command; state lives in `~/.local/state/xm5/anc`

## Mic enhancement (noise suppression + auto gain)

`xm5 mic-enhance on` loads PipeWire `module-echo-cancel` with WebRTC audio
processing: noise suppression, high-pass filter (removes XM5 rumble/blower),
digital AGC. It creates a new source `xm5_enhanced_mic` and makes it default —
**every browser and every app** that uses the pulse default source
automatically gets the improved mic. In music mode (no mic available) it is
deactivated automatically.

## Automation (pilot service)

`xm5-pilot.service` (user unit) monitors every 2 s:
- Headset disappears → mic enhancement is unloaded (no dead mic nodes for browsers)
- Headset reconnects → desired mode (from the last TUI/CLI action) is restored and default sink/source are set correctly
- `xm5 mode auto` turns off profile forcing (only repair defaults)

Logs: `~/.config/xm5/pilot.log` · mode state: `~/.local/state/xm5/mode`

## Dependencies

- `pactl` (pipewire-pulse or pulseaudio) — present by default on Omarchy & Ubuntu
- `bluetoothctl` (bluez)
- Python 3 with `curses` (standard)

Rust and cargo are **optional**: without them the bundled static `xm5-tui` is
installed as-is.

### Offline builds

`xm5-tui/vendor.tar.xz` is a snapshot of every crate in `Cargo.lock` (generated
with `cargo vendor --locked`), so `cargo build --release --locked --offline`
works with no registry access. For manual builds:

```sh
cd xm5-tui
mkdir -p ~/.cache/xm5 && tar -xJf vendor.tar.xz -C ~/.cache/xm5
cargo build --release --locked --offline \
  --config 'source.crates-io.replace-with="vendored-sources"' \
  --config "source.vendored-sources.directory=\"$HOME/.cache/xm5/vendor\""
```

Refresh the snapshot after changing `Cargo.toml`/`Cargo.lock` (the shipped
archive has platform `.a`/`.lib`/`.dll` blobs removed — they are Windows-only
and would otherwise double the size):

```sh
cd xm5-tui
rm -rf /tmp/xm5-vendor && mkdir -p /tmp/xm5-vendor
cargo vendor --locked /tmp/xm5-vendor/vendor
python3 - <<'EOF'
import json, pathlib
for cj in pathlib.Path('/tmp/xm5-vendor/vendor').glob('*/.cargo-checksum.json'):
    blobs = [f for f in cj.parent.rglob('*')
             if f.suffix in {'.a', '.lib', '.dll', '.exe', '.pdb', '.rlib'}]
    if not blobs:
        continue
    d = json.loads(cj.read_text())
    for f in blobs:
        d.get('files', {}).pop(f.relative_to(cj.parent).as_posix(), None)
        f.unlink()
    cj.write_text(json.dumps(d))
EOF
tar -cJf vendor.tar.xz -C /tmp/xm5-vendor vendor && rm -rf /tmp/xm5-vendor
```

## Uninstall

```sh
systemctl --user disable --now xm5-pilot.service
rm ~/.config/systemd/user/xm5-pilot.service
rm ~/.local/bin/xm5*
rm -rf ~/.cache/xm5
# remove the Hyprland block in ~/.config/hypr/bindings.lua ("XM5 headset")
```