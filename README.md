# xm5 – Sony WH-1000XM5 Controller für Linux

TUI + CLI für die Sony WH-1000XM5 (funktioniert mit jedem BT-Headset): Profilwechsel
Musik ⇄ Mikrofon, Lautstärke, Mikrofon-Enhancement, Reparatur, Akku.
Getestet auf Omarchy (Arch + PipeWire + Hyprland); läuft auch unter Ubuntu
(PipeWire **oder** PulseAudio – beides wird über `pactl` abgedeckt).

## Warum das nötig ist

Im Bluetooth-Musikmodus (A2DP) gibt es kein Headset-Mikrofon. Normale Desktops
schalten dann automatisch auf HFP um, sobald irgendein Client das Mikrofon
anfragt (inklusive WebRTC-Geräte-Scans im Browser!) — jede Umschaltung ist als
Disconnect-Aussetzer hörbar, und XM5-Jingles nerven.

Der Fix in WirePlumber: `bluez5.autoswitch-to-headset-profile = false`
(`~/.config/wireplumber/wireplumber.conf.d/51-no-xm5-autoswitch.conf`).
Das XM5-Mikrofon läuft danach als **A2DP-Source parallel** (16 kHz, volle
Musik-Qualität bleibt) — Browser/Calls erkennen es sofort, ohne Profilflip.
Der klassische HFP-Modus bleibt manuell per TUI (`V`) oder `xm5 mode mic`.


## Installation

```sh
git clone https://github.com/34t0th3g4me/xm5.git
cd xm5
python3 install.py
```

Das installiert alles: `xm5` in `~/.local/bin`, die systemd-User-Unit und die
Hyprland-Shortcuts (nur Omarchy). Rust-Toolchain wird automatisch benutzt, wenn
vorhanden (`xm5-tui` wird gebaut); sonst liegt nur die Python-Engine + CLI da.

Das macht:
1. Rust-TUI: `~/.local/bin/xm5-tui-bin` (+ Wrapper `xm5-tui`) — gebaut falls nötig
2. `~/.local/bin/xm5` (+ `xm5-music`, `xm5-mic`, `xm5-toggle`, `xm5-volup`, `xm5-voldown`, `xm5-repair`)
3. systemd-User-Unit `xm5-pilot.service` (hält Profil + Default-Senke/Quelle auch nach Reconnects stabil, loggt in `~/.config/xm5/pilot.log`)
4. Hyprland-Shortcuts (nur Omarchy):
   - `SUPER + ALT + M` → TUI öffnen (wiederholt: fokussieren)
   - `SUPER + ALT + SHIFT + UP/DOWN` → Lautstärke ±5 % (wiederholbar)
   - `SUPER + ALT + SHIFT + R` → Reparatur
   - (Plain `SUPER+ALT+UP/DOWN` bleibt für Omarchy's Fenster-Gruppen belegt)

Unter Ubuntu/GNOME: TUI einfach über `xm5` im Terminal starten; die Shortcuts
kannst du unter Einstellungen › Tastatur › Kürzel anlegen (`xm5-toggle`,
`xm5-volup`, `xm5-voldown`).

## TUI (Nothing-X-Look, 2-spaltig, Maus)

```sh
xm5          # öffnet das TUI-Dashboard (xm5-tui)
xm5-tui      # direkt
```

Dichtes 2-Spalten-Dashboard im Nothing-X-Stil (near-black, graue Kacheln,
runde Borders, weiße aktive Pods, grüner Status-Dot):

```
 ● WH-1000XM5  auto    󰂁 80%
╭ device ────────────╮╭ noise cancel ────────────────╮
│ 󰋎 SONY WH-1000XM5  ││   ╭─────╮   ╭─────╮  ╭─────╮  │
│ mode music [aac]   ││   │ 󰆩 │   │ 󰙅 │  │ 󰋐 │  │
│ 󰂯 verbunden · …    ││   ╰─────╯   ╰─────╯  ╰─────╯  │
╰────────────────────╯│ noise cancel  ambient   off   │
                      │  15 ━━━━━━━━━━━━━━━  54%      │
╭ volume ────────────╮╭ quick ────────────────────────╮
│ 󰕾 80%              ││ [M] music [V] mic [T] toggle  │
│  ┃┃┃┃┃┃┃┃┃┃┃┃━━    ││ [E] dsp   [R] repair    auto  │
╰────────────────────╯╰───────────────────────────────╯
╭ mic DSP ●╮ ╭ repair bluetooth ╮ ╭ ● auto pilot ╮
```

**Maus**: Pods anklicken (ANC/Ambient/Off), in der Volumen-Tick-Leiste
klicken+ziehen (direkte Position), Mausrad = Volume ±3, alle Chips
anklickbar (auch `auto` = Pilot-Modus umschalten).

**Tasten**: `1`/`2`/`3` Pods · `←`/`→` Pods · `↑`/`↓` ANC-Level (1-28) ·
`+`/`-` Volume · `m/v/t` Musik/Mikro/Toggle · `e` mic-DSP · `r` Reparatur ·
`F` Refresh · `q` Beenden

**Layout**: Responsive — 78×13+ zweispaltig, schmaler einspaltig gestapelt
(device · ANC · volume · quick · Mikrofon). Die Kachel-Höhen wachsen
rund-robin (Volume → ANC → Mic → Device → Quick). Der Mikrofon-Skop zeigt
die ankommende XM5-Spur live (pw-cat-Capture, ~8 fps, dB + letzter Pegel
in Akzentblau); bei 'kein gerät' oder inaktivem Mic erscheint der Hinweis
zum Mic-Modus. Footer minimal (q/? ) — alle Tasten im '?'-Overlay.

**Design/Animationen**: Start-Stagger (Kachel-Borders blitzen akzentblau
gestaffelt), Pod-Click-Flash (350 ms), Volume-Änderungen glühen im Balken
(Akzent → Gradient), Status-Dot atmet (2,4 s), Toast mit Akzent-Pop-in +
Fade-out. Alle Icons sind gegen die tatsächlich installierte Nerd-Font
verifiziert (Pango-Render + cmap): 󰖔 ANC (Mond), 󰞍 Wellen (Ambient),
󰋐 Headset-Off, 󰝚 Musik, 󰓡 Swap, 󰘮 Tuner (DSP), 󰒓 Zahnrad (Repair),
󰁨 Auto-Fix (Pilot), 󰂯 Bluetooth, Battery-Leiter. Respektiert `NO_COLOR`.

**Latenz**: Pod- und Level-Klicks greifen sofort (optimistisch); die XM5-
Bestätigung läuft im Hintergrund (kein UI-Block). Status-Refresh nur bei
Änderung (der Pilot-Service schreibt `~/.local/state/xm5/status.json` alle 2 s;
das TUI liest die Datei statt Prozesse zu starten) — idle-CPU ≈ 0,2 % statt
1,3 %. Layout skaliert von 52×14 bis Vollbild.

## UI-Sounds

`xm5` spielt kurze synth-Jingles beim Moduswechsel (`sounds/*.wav`):
`music`, `mic`, `connected`, `power_on`, `power_off`, `error`.
Für ANC/Volume **keine PC-Sounds** — die XM5 spielt dafür selbst ihren
Geräte-Jingle (sonst doppelt). Ausschalten mit `XM5_SOUNDS=0`.

## Bekanntes Problem (behoben)

Sony WH-1000XM5 stirbt beim Profilwechsel gern inside: der HFP-SCO-Transport
lieft dann nur Nullen (Mik "nichts ankommen"), obwohl alles "verbunden" aussieht.
`xm5` erkennt das seit 2026-10-05 automatisch:
- `xm5 mode mic` und `xm5 repair` prüfen nach dem Umschalten die Mikrofon-Daten (`mic_alive`)
- Bei totem SCO-Link folgt automatisch ein Disconnect/Reconnect + Profil-Reset
- Danach liefern der Browser und pw-record wieder echte Samples.

Falls es hängt: einfach `xm5 repair` (oder TUI-Taste `R`).

## CLI

```sh
xm5 status                 # JSON-Status (Profil, Codec, Volume, Akku)
xm5 mode music             # A2DP AAC (Musik, kein Mic)
xm5 mode mic               # HSP/HFP MSBC (Mic aktiv)
xm5 mode toggle            # umschalten
xm5 mode auto              # Pilot-Service aktivieren (defaults repair)
xm5 volume + | - | 90 | max | set 70
xm5 mic-enhance on|off|toggle|status
xm5 anc on|off             # Noise Cancelling an/aus (direkt am XM5, RFCOMM)
xm5 anc ambient 12         # Ambient-Sound-Modus, Pegel 1-28
xm5 anc voice 10           # Voice-Focus (Umgebung + Stimmen)
xm5 anc status             # letzter gesetzter Zustand
xm5 battery
xm5 repair                 # kaputte BT-Audio-Verbindung reparieren
xm5 discover               # gepaarte BT-Geräte anzeigen
```

## Noise Cancelling / Ambient Sound

Läuft über Sony's proprietäres RFCOMM-Protokoll (Kanal 9, Service
`956c7b26-…`) - gleicher Weg wie die offizielle App:
- `A` im TUI = ANC an, `C` = ANC/ASM komplett aus, `V` = Voice-Focus
- `xm5 anc ambient <level>` = Umgebung mit einstellbarem Pegel (1-28)
- Der XM5 bestätigt jeden Befehl mit ACK; Zustand liegt in `~/.local/state/xm5/anc`

## Mikrofon-Enhancement (Rauschunterdrückung + Auto-Gain)

`xm5 mic-enhance on` lädt PipeWire `module-echo-cancel` mit WebRTC-Audio-Processing:
Noise-Suppression, High-Pass-Filter (tastert RUMS/Blower der XM5 weg), Digital-AGC.
Das erzeugt eine neue Quelle `xm5_enhanced_mic` und setzt sie als Default – **jeder
Browser und jede App**, die das Pulsesource-Default nimmt, bekommt automatisch das
verbesserte Mic. Im Musik-Modus (kein Mic verfügbar) wird es automatisch deaktiviert.

## Automatik (Pilot-Service)

`xm5-pilot.service` (User-Unit) überwacht alle 2 s:
- Headset verschwindet → Mic-Enhancement wird entladen (keine toten Mic-Nodes für Browser)
- Headset reconnectet → gewünschter Modus (aus letzter TUI-/CLI-Aktion) wird wiederhergestellt und Default-Sink/-Source korrekt gesetzt
- `xm5 mode auto` schaltet die Profil-Erzwingung aus (nur Defaults reparieren)

Logs: `~/.config/xm5/pilot.log` · Modus-State: `~/.local/state/xm5/mode`

## Abhängigkeiten

- `pactl` (pipewire-pulse oder pulseaudio) – auf Omarchy & Ubuntu standardmäßig drauf
- `bluetoothctl` (bluez)
- Python 3 mit `curses` (Standard)

## Deinstallation

```sh
systemctl --user disable --now xm5-pilot.service
rm ~/.config/systemd/user/xm5-pilot.service
rm ~/.local/bin/xm5*
# Hyprland-Block in ~/.config/hypr/bindings.lua löschen ("XM5 headset")
```