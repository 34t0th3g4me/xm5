#!/usr/bin/env python3
# xm5 - Sony WH-1000XM5 profile/volume/mic helper for Linux (PipeWire/PulseAudio).
# Omarchy (Arch/Hyprland) and Ubuntu supported. Dependencies: pactl, bluetoothctl.
# Run without args for the interactive TUI. See README.md.

import json
import os
import re
import shutil
import fcntl
import socket
import struct
import subprocess
import sys
import time
from pathlib import Path

CONFIG_DIR = Path.home() / ".config" / "xm5"
CONFIG_FILE = CONFIG_DIR / "config.json"
LOG_FILE = CONFIG_DIR / "pilot.log"
STATE_DIR = Path.home() / ".local" / "state" / "xm5"

UNIT_DIR = Path.home() / ".config/systemd/user"
UNIT_NAME = "xm5-pilot.service"
UNIT_FILE = UNIT_DIR / UNIT_NAME

DEFAULTS = {"step": 5, "enhance": True}

MUSIC_PROFILES = [("a2dp-sink", "Music (A2DP, AAC) - best quality"),
                  ("a2dp-sink-sbc_xq", "Music (A2DP, SBC-XQ)"),
                  ("a2dp-sink-sbc", "Music (A2DP, SBC)")]
MIC_PROFILES = [("headset-head-unit", "Calls (HSP/HFP, mSBC mic)"),
                ("headset-head-unit-cvsd", "Calls (HSP/HFP, CVSD mic)")]

# ---------------------------------------------------------------- helpers



LOCK_FILE = STATE_DIR / "profile.lock"


class ProfileLock:
    """Serialize every profile/ANC-modifying operation between CLI/TUI and pilot."""

    def __init__(self):
        self.fd = None

    def __enter__(self):
        STATE_DIR.mkdir(parents=True, exist_ok=True)
        self.fd = open(LOCK_FILE, "w")
        # bounded wait: a stale holder (killed CLI, hung TUI) must not wedge
        # the pilot or a CLI call forever. 10 s then fail loudly.
        deadline = time.monotonic() + 10.0
        while True:
            try:
                fcntl.flock(self.fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                return self
            except BlockingIOError:
                if time.monotonic() >= deadline:
                    self.fd.close()
                    self.fd = None
                    raise TimeoutError("profile.lock: another xm5 op holds the lock (10 s)")
                time.sleep(0.2)

    def __exit__(self, *exc):
        try:
            if self.fd:
                fcntl.flock(self.fd, fcntl.LOCK_UN)
                self.fd.close()
        except Exception:
            pass
        return False


def log(msg):
    try:
        CONFIG_DIR.mkdir(parents=True, exist_ok=True)
        with open(LOG_FILE, "a") as f:
            f.write("%s %s\n" % (time.strftime("%Y-%m-%d %H:%M:%S"), msg))
    except OSError:
        pass


def run(cmd, check=True):
    return subprocess.run(cmd, capture_output=True, text=True, check=check).stdout


def run_ok(cmd):
    r = subprocess.run(cmd, capture_output=True, text=True)
    return r.returncode, r.stdout, r.stderr


def notify(summary, body=""):
    if shutil.which("notify-send"):
        subprocess.Popen(["notify-send", "-a", "xm5", "-t", "4000", summary, body],
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                         start_new_session=True)
    else:
        print(summary + (" - " + body if body else ""))


def die(msg, code=1):
    print("xm5: " + msg, file=sys.stderr)
    sys.exit(code)


def load_cfg():
    cfg = dict(DEFAULTS)
    try:
        cfg.update(json.loads(CONFIG_FILE.read_text()))
    except (OSError, ValueError):
        pass
    return cfg


def save_cfg(cfg):
    CONFIG_DIR.mkdir(parents=True, exist_ok=True)
    CONFIG_FILE.write_text(json.dumps(cfg, indent=2) + "\n")

# ---------------------------------------------------------------- bluetooth


def bt_device_list():
    rc, out, _ = run_ok(["bluetoothctl", "devices", "Paired"])
    if rc != 0:
        out = run(["bluetoothctl", "devices"])
    devs = []
    for line in out.splitlines():
        m = re.match(r"Device (\S+) (.+)$", line.strip())
        if m:
            devs.append({"mac": m.group(1), "name": m.group(2)})
    return devs


def bt_info(mac):
    out = run(["bluetoothctl", "info", mac])
    d = {"connected": False, "name": "", "battery": None}
    for line in out.splitlines():
        line = line.strip()
        if line.startswith("Name:"):
            d["name"] = line.split(":", 1)[1].strip()
        elif line.startswith("Connected:"):
            d["connected"] = line.split(":", 1)[1].strip() == "yes"
        elif line.startswith("Battery Percentage:"):
            m = re.search(r"\((\d+)\)", line)
            if m:
                d["battery"] = int(m.group(1))
    return d


def bt_connect(mac):
    run(["bluetoothctl", "connect", mac], check=False)
    for _ in range(20):
        if bt_info(mac).get("connected"):
            return True
        time.sleep(0.5)
    return False

# ---------------------------------------------------------------- pa/pipewire


def pa_cards():
    try:
        return json.loads(run(["pactl", "-f", "json", "list", "cards"]))
    except (ValueError, subprocess.CalledProcessError, FileNotFoundError):
        return []


def headset_cards():
    """Bluetooth audio cards matching our headset (or all BT cards if none claimed)."""
    cards = []
    for c in pa_cards():
        props = c.get("properties", {})
        if props.get("device.api") == "bluez5":
            cards.append(c)
    return cards


def card_of(mac=None):
    want = (mac or "").replace(":", "_").replace(".", "_").lower()
    for c in headset_cards():
        name = c["name"]
        n = name.lower()
        if want and want in n:
            return c
    if not want:
        return headset_cards()[0] if headset_cards() else None
    # fallback: any connected BT card
    for c in headset_cards():
        if c.get("properties", {}).get("connection") in ("connected", "1"):
            return c
    return headset_cards()[0] if headset_cards() else None


def card_profiles(card):
    """Profiles is a dict {name: {description, available, ...}} in pactl json."""
    out = []
    for key, meta in card.get("profiles", {}).items():
        out.append({"name": key,
                    "description": meta.get("description", key),
                    "available": bool(meta.get("available", True))})
    return out


def active_profile(card):
    return card.get("active_profile", "") or card.get("properties", {}).get("bluez5.profile") or ""


def set_profile(card_name, profile):
    with ProfileLock():
        rc, out, err = run_ok(["pactl", "set-card-profile", card_name, profile])
    if rc != 0:
        return False, (err or out).strip()
    # verify
    time.sleep(0.4)
    for card in headset_cards():
        if card["name"] == card_name:
            return active_profile(card) == profile, ""
    return False, "card vanished"


def is_music_profile(p):
    return p.startswith("a2dp")


def headset_nodes(mac=None):
    """Return (sink_names, source_names) for the headset nodes."""
    sinks, sources = [], []
    want = (mac or "").replace(":", "_").replace(".", "_").lower()
    try:
        sl = json.loads(run(["pactl", "-f", "json", "list", "sinks"]))
        for s in sl:
            if "bluez" in s.get("name", ""):
                sinks.append(s["name"])
    except (ValueError, subprocess.CalledProcessError):
        pass
    try:
        sl = json.loads(run(["pactl", "-f", "json", "list", "sources"]))
        for s in sl:
            if "bluez" in s.get("name", ""):
                sources.append(s["name"])
    except (ValueError, subprocess.CalledProcessError):
        pass
    return sinks, sources


def set_default_sink(name):
    run(["pactl", "set-default-sink", name])
    run(["pactl", "set-sink-mute", name, "0"])


def set_default_source(name):
    run(["pactl", "set-default-source", name])


def sink_volume(name):
    try:
        data = json.loads(run(["pactl", "-f", "json", "list", "sinks"]))
        for s in data:
            if s["name"] == name:
                vol = s.get("volume", {})
                first = next(iter(vol.values()), {})
                pct = first.get("value_percent", "100%")
                return int(str(pct).rstrip("%") or 100), bool(s.get("mute"))
    except (ValueError, subprocess.CalledProcessError):
        pass
    return None, None


def change_volume(name, delta=None, target=None):
    if target is not None:
        run(["pactl", "set-sink-volume", name, "%d%%" % max(0, min(100, target))])
    elif delta is not None:
        run(["pactl", "set-sink-volume", name, "%s%d%%" % ("+" if delta >= 0 else "-", abs(delta))])
    return sink_volume(name)

# ---------------------------------------------------------------- sony ANC/ambient sound

SONY_MAC = None  # discovered lazily


def _sony_escape(data: bytes) -> bytes:
    out = bytearray()
    for b in data:
        if b == 60:
            out += b"\x3d\x2c"      # 61, 44
        elif b == 61:
            out += b"\x3d\x2d"      # 61, 45
        elif b == 62:
            out += b"\x3d\x2e"      # 61, 46
        else:
            out.append(b)
    return bytes(out)


def _sony_frame(payload: bytes, seq: int = 0, dtype: int = 12) -> bytes:
    body = bytearray([dtype, seq]) + struct.pack(">I", len(payload)) + payload
    body.append(sum(body) & 0xFF)
    return b"\x3e" + _sony_escape(bytes(body)) + b"\x3c"


def sony_headset_mac():
    """MAC of a connected Sony headset (WH-1000XM5 etc)."""
    for d in bt_device_list():
        info = bt_info(d["mac"])
        if info["connected"] and ("xm" in d["name"].lower() or "sony" in d["name"].lower()):
            return d["mac"]
    return None


def sony_send(command: bytes, channel: int = 9, timeout: float = 2.0):
    """Send a framed Sony command over RFCOMM; returns (ok, ack_bytes|error)."""
    mac = sony_headset_mac()
    if not mac:
        return False, "sony headset not connected"
    sock = socket.socket(socket.AF_BLUETOOTH, socket.SOCK_STREAM, socket.BTPROTO_RFCOMM)
    sock.settimeout(timeout)
    try:
        sock.connect((mac, channel))
        sock.sendall(_sony_frame(command))
        try:
            ack = sock.recv(120)
        except socket.timeout:
            ack = b""
        return True, ack
    except Exception as e:
        return False, str(e)
    finally:
        try:
            sock.close()
        except OSError:
            pass


def sony_session(timeout: float = 4.0):
    """Open an RFCOMM command session to the Sony headset.
    Returns (socket, seq) or (None, error)."""
    mac = sony_headset_mac()
    if not mac:
        return None, "sony headset not connected"
    try:
        sock = socket.socket(socket.AF_BLUETOOTH, socket.SOCK_STREAM, socket.BTPROTO_RFCOMM)
        sock.settimeout(timeout)
        sock.connect((mac, 9))
        return sock, mac
    except Exception as e:
        return None, str(e)


def _select_read(sock, timeout):
    import select as _s
    try:
        return bool(_s.select([sock], [], [], timeout)[0])
    except Exception:
        return False


def sony_pump(sock, seconds: float):
    """Receive DATA frames and ACK each immediately (XM5 retransmits without ACKs)."""
    seen = []
    end_at = time.time() + seconds
    buf = b""
    while time.time() < end_at:
        if not _select_read(sock, 0.12):
            continue
        try:
            d = sock.recv(700)
        except (socket.timeout, OSError):
            break
        if not d:
            break
        buf += d
        i = 0
        while i < len(buf) - 8:
            if buf[i:i + 1] == b"\x3e" and buf[i + 1:i + 2] == b"\x0c":
                j = buf.find(b"\x3c", i)
                if j < 0:
                    break
                rxseq = buf[i + 2]
                payload = buf[i + 7:j]
                try:
                    sock.sendall(_sony_frame(b"", seq=(1 - rxseq) & 0xFF, dtype=1))
                except OSError:
                    pass
                seen.append(payload)
                i = j + 1
            else:
                i += 1
        buf = buf[i:] if i > 0 else buf
    return seen


def _sony_command(sock, payload: bytes, seq: int, dtype: int = 12):
    sock.sendall(_sony_frame(payload, seq=seq, dtype=dtype))


ANC_TYPE = 0x17  # XM5 capability: MODE_NC_ASM_DUAL_NC_MODE_SWITCH_AND_ASM_SEAMLESS


def anc_set(mode: str, level: int = 20):
    """ANC/ASM control for XM5 (V2 protocol, NCASM_SET_PARAM 0x68, type 0x17 layout):
    [0x68, 0x17, CHANGED, totalEffect, ncAsmMode (0=NC 1=ASM), asmSoundMode (0=normal
    1=voice), ambientLevel]. level 1-28 (0 disables ambient level semantics for off)."""
    mac = sony_headset_mac()
    if not mac:
        return False, "sony headset not connected"
    sock = None
    try:
        sock, msg = sony_session()
        if sock is None:
            return False, msg
        # handshake: the XM5 only processes commands after CONNECT_GET_PROTOCOL_INFO
        sock.sendall(_sony_frame(bytes([0x00, 0x00]), seq=0))  # CONNECT_GET_PROTOCOL_INFO
        got = sony_pump(sock, 1.4)
        seq = 1
        if mode == "on":
            payload = bytes([0x68, ANC_TYPE, 0x01, 0x01, 0x00, 0x00, 15])
        elif mode == "off":
            payload = bytes([0x68, ANC_TYPE, 0x01, 0x00, 0x00, 0x00, 15])
        elif mode == "ambient":
            payload = bytes([0x68, ANC_TYPE, 0x01, 0x01, 0x01, 0x00, max(0, min(28, level))])
        elif mode == "voice":
            payload = bytes([0x68, ANC_TYPE, 0x01, 0x01, 0x01, 0x01, max(0, min(28, level))])
        else:
            return False, "mode must be on|off|ambient|voice"
        sock.sendall(_sony_frame(payload, seq=seq, dtype=12))
        seq += 1
        resp = sony_pump(sock, 2.4)
        # XM5 notifies applied state via NCASM_NTFY_PARAM (0x69) or similar
        if resp:
            kind = resp[0][0]
            what = "ack" if resp[0] == b"" else {0x69: "applied", 0x67: "ret-param",
                                                 0x63: "ret-status"}.get(kind, "resp")
            return True, "ok (%s: %s)" % (what, resp[0].hex())
        return True, "ok (ack only)"
    except Exception as e:
        return False, str(e)
    finally:
        if sock:
            try:
                sock.close()
            except OSError:
                pass


ANC_STATE = STATE_DIR / "anc"


def anc_state_file_write(mode: str, level: int):
    try:
        STATE_DIR.mkdir(parents=True, exist_ok=True)
        ANC_STATE.write_text("%s %d\n" % (mode, level))
    except OSError:
        pass


def anc_state():
    try:
        mode, lvl = ANC_STATE.read_text().split()
        return {"anc": mode, "level": int(lvl)}
    except (OSError, ValueError):
        return {"anc": None, "level": None}


def anc_availability():
    """Live from the headset: NCASM_GET_STATUS (0x62 / type NC_ON_OFF=0x01).
    True if the headset currently has ANC/ASM control available (0x00 = ENABLE)."""
    try:
        sock, msg = sony_session()
        if sock is None:
            return None
        sock.sendall(_sony_frame(bytes([0x00, 0x00]), seq=0))
        sony_pump(sock, 1.2)
        sock.sendall(_sony_frame(bytes([0x62, 0x01]), seq=1))
        frames = sony_pump(sock, 1.6)
        for p in frames:
            if p[:1] == b"\x63" and len(p) >= 3:
                return p[2] == 0x00  # ENABLE = 0x00
        return None
    except Exception:
        return None
    finally:
        try: sock.close()
        except OSError: pass


def cmd_anc(args):
    what = args[0] if args else "status"
    if what in ("on", "off", "ambient", "voice"):
        lvl = int(args[1]) if len(args) > 1 and args[1].isdigit() else anc_state().get("level") or 20
        ok, msg = anc_set(what, lvl)
        if ok:
            anc_state_file_write(what, lvl)
        print("%s: %s" % (what, msg))
        sys.exit(0 if ok else 1)
    if what == "status":
        st = anc_state()
        avail = anc_availability()
        st["nc_available"] = avail
        print(json.dumps(st))
        return
    print("usage: xm5 anc <on|off|ambient|voice|status> [level1-28]")
    sys.exit(1)




# ---------------------------------------------------------------- ui sounds
SOUND_DIR = Path(__file__).resolve().parent / "sounds"
_SOUND_ON = None


def sound_pref():
    global _SOUND_ON
    if _SOUND_ON is None:
        _SOUND_ON = os.environ.get("XM5_SOUNDS", "1") != "0"
    return _SOUND_ON


def play_sound(name: str):
    """Best-effort UI jingle via paplay; never raises, skips when disabled."""
    if not sound_pref():
        return
    f = SOUND_DIR / (name + ".wav")
    if not f.exists():
        return
    try:
        subprocess.Popen(["paplay", str(f)],
                         stdout=subprocess.DEVNULL,
                         stderr=subprocess.DEVNULL)
    except Exception:
        pass

# ---------------------------------------------------------------- repair



def mic_alive(mac, seconds=1.6):
    """True when the BT source delivers real non-silence data (detecting a dead SCO transport).
    Records briefly into a temp wav via pw-record and checks the peak."""
    try:
        path = "/tmp/xm5_mic_probe.wav"
        if os.path.exists(path):
            os.unlink(path)
        try:
            subprocess.run(["pw-record", "--target", "bluez_input.%s" % mac.replace(":", "-"),
                            path], timeout=seconds,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        except subprocess.TimeoutExpired:
            pass  # expected: pw-record runs until killed; the partial wav is what we probe
        if not os.path.exists(path) or os.path.getsize(path) < 4000:
            return False
        # killed pw-record leaves raw PCM without finished wav header - read directly
        data = open(path, "rb").read()
        vals = struct.unpack("<%dh" % (len(data) // 2), data[:len(data) // 2 * 2])
        if not vals:
            return False
        # dead SCO transports deliver hard zeros (exact 0, not even dither noise)
        nonzero = sum(1 for v in vals if abs(v) > 8)
        return nonzero > len(vals) / 200  # >0.5% non-zero = real signal path alive
    except Exception:
        return False

def repair(connect=False):
    """Fix broken bluetooth audio. Escalation ladder:
    1. profile bounce (off -> previous)  2. full BT disconnect+reconnect.
    Reloads mic-enhancement afterwards so its capture stream binds the fresh mic."""
    log("repair requested")
    was_enhanced = enhance_status()
    if was_enhanced:
        enhance_deactivate()
    cards = headset_cards()
    if not cards:
        devs = bt_device_list()
        hset = next((d for d in devs if "xm" in d["name"].lower() or "sony" in d["name"].lower()), None) \
            or (devs[0] if devs else None)
        if not hset:
            return False, "no paired BT audio device found"
        if not bt_connect(hset["mac"]):
            return False, "could not connect to %s" % hset["name"]
        cards = headset_cards()
        if not cards:
            return False, "connected but no BT audio card appeared (check pipewire-pulse)"
    card = cards[0]
    name = card["name"]
    prof = active_profile(card)
    # step 1: profile bounce
    set_profile(name, "off")
    time.sleep(0.6)
    target = prof if prof and prof != "off" else "a2dp-sink"
    ok, err = set_profile(name, target)
    if not ok:
        return False, err or "profile %s rejected" % target
    time.sleep(0.6)
    sinks, sources = headset_nodes()
    if sinks:
        set_default_sink(sinks[0])
    mac = card.get("properties", {}).get("device.string", "").strip()
    mic_dead = bool(sources) and not mic_alive(mac)
    if not sources or mic_dead:
        # step 2: no mic/source nodes OR dead SCO transport (null-silence) -> full reconnect
        if mic_dead:
            log("repair escalation: mic alive-check failed (dead SCO) -> reconnect")
        else:
            log("repair escalation: full reconnect")
        if mac:
            run(["bluetoothctl", "disconnect", mac], check=False)
            time.sleep(1.5)
            bt_connect(mac)
            card = card_of(mac)
            target = prof if prof and prof != "off" else "a2dp-sink"
            if card:
                set_profile(card["name"], target)
            time.sleep(1.0)
    sinks, sources = headset_nodes()
    if sinks:
        set_default_sink(sinks[0])
    if sources:
        set_default_source(sources[0])
    # after full reconnect, re-assert target profile and verify mic liveness
    if mic_dead:
        time.sleep(1.2)
        if not mic_alive(mac):
            time.sleep(2.0)
    # reload enhancement on the fresh mic
    if was_enhanced and load_cfg().get("enhance", True):
        enhance_activate()
    return True, "%s profile=%s sink=%s" % (name, target, (sinks[0] if sinks else "-"))


def goto_mode(mode):
    """mode: music | mic | toggle. Writes desired mode state + keeps
    mic-enhancement consistent with mode."""
    cards = headset_cards()
    if not cards:
        return False, "no BT audio card; run: xm5 repair"
    card = cards[0]
    name = card["name"]
    cur = active_profile(card)
    if mode == "toggle":
        mode = "mic" if is_music_profile(cur) else "music"
    STATE_DIR.mkdir(parents=True, exist_ok=True)
    (STATE_DIR / "mode").write_text(mode + "\n")
    chain = MUSIC_PROFILES if mode == "music" else MIC_PROFILES
    last_err = "unknown mode %s" % mode
    for prof, _desc in chain:
        ok, err = set_profile(name, prof)
        if ok:
            time.sleep(0.8)
            sinks, sources = headset_nodes()
            if sinks:
                set_default_sink(sinks[0])
            # XM5-Hack: after profile switch the SCO transport can be dead (silence-only).
            # Detect and fix via full reconnect once, then re-assert profile.
            mac2 = name.replace("bluez_card.", "").replace("_", ":")
            if mode == "mic" and not mic_alive(mac2):
                with ProfileLock():
                    log("goto_mode: dead SCO after profile switch -> reconnect")
                    run(["bluetoothctl", "disconnect", mac2], check=False)
                    time.sleep(1.5)
                    bt_connect(mac2)
                    time.sleep(1.0)
                    ok2, _ = set_profile(name, prof)
                    if ok2:
                        time.sleep(1.0)
                        sinks, sources = headset_nodes()
                        if sinks:
                            set_default_sink(sinks[0])
            # enhancement only makes sense while a mic exists
            if mode == "music":
                if enhance_status():
                    ok2, msg2 = enhance_deactivate()
                    log("enhance auto-off on music: %s" % msg2)
            elif load_cfg().get("enhance"):
                enhance_activate()
            return True, "%s -> %s" % (mode, prof)
        last_err = err
    return False, last_err

# ---------------------------------------------------------------- mic enhancement

ENH_UNIT = "xm5-enhance.service"
ENH_SOURCE = "echo-cancel-source"


def enhance_args():
    return ("aec_args=webrtc.high_pass_filter=1 "
            "webrtc.noise_suppression=2 "
            "webrtc.gain_control=1 "
            "webrtc.analog_gain_control=0 "
            "webrtc.digital_gain_control=1")


ENH_LOCK = Path("/tmp/xm5-enh.lock")


def _enh_lock():
    """Simple lock so CLI TUI and pilot never manipulate the module concurrently."""
    for _ in range(40):
        try:
            CONFIG_DIR.mkdir(parents=True, exist_ok=True)
            fd = os.open(str(ENH_LOCK), os.O_CREAT | os.O_EXCL | os.O_WRONLY)
            os.write(fd, str(os.getpid()).encode())
            os.close(fd)
            return
        except FileExistsError:
            time.sleep(0.25)
    return


def _enh_unlock():
    try:
        ENH_LOCK.unlink(missing_ok=True)
    except OSError:
        pass


def _enh_modules():
    rc, out, _ = run_ok(["pactl", "list", "short", "modules"])
    return [l.split("\t")[0] for l in out.splitlines() if "module-echo-cancel" in l]


def enhance_activate():
    """Load module-echo-cancel reading the headset mic, exposing it as the
    default source with webrtc noise suppression + AGC + high-pass filter.
    Idempotent + locked: pilot and CLI can call concurrently."""
    _enh_lock()
    try:
        mods = _enh_modules()
        for m in mods:
            run(["pactl", "unload-module", m])
        if mods:
            time.sleep(0.4)
        sinks, sources = headset_nodes()
        mic = next((s for s in sources if s.startswith("bluez_input")), None)
        rc, out, err = run_ok(["pactl", "load-module", "module-echo-cancel",
                               "source_name=xm5_enhanced_mic",
                               "source_properties=device.description=XM5-Mic-Enhanced",
                               "aec_args=webrtc.high_pass_filter=1 "
                               "webrtc.noise_suppression=2 "
                               "webrtc.gain_control=1"])
        if rc != 0:
            return False, (err or out).strip()
        time.sleep(0.8)
        # the capture stream auto-connects to the wrong source (first available mic);
        # move every echo-cancel-capture stream to the headset mic and verify, retrying
        # until it sticks (autoconnect races with the move right after load).
        # pactl requires numeric indices here (names fail with 'No such entity').
        if mic:
            mic_id = None
            try:
                srcs = json.loads(run(["pactl", "-f", "json", "list", "sources"]))
                mic_id = next(str(s["index"]) for s in srcs if s.get("name") == mic)
            except (ValueError, subprocess.CalledProcessError, StopIteration):
                pass
            if mic_id:
                for attempt in range(5):
                    try:
                        outs = json.loads(run(["pactl", "-f", "json", "list", "source-outputs"]))
                        wrong = [so["index"] for so in outs
                                 if so.get("properties", {}).get("node.name") == "echo-cancel-capture"
                                 and str(so.get("source")) != mic_id]
                        for idx in wrong:
                            run(["pactl", "move-source-output", str(idx), mic_id])
                        if not wrong:
                            break
                    except (ValueError, subprocess.CalledProcessError):
                        pass
                    time.sleep(0.6)
        set_default_source("xm5_enhanced_mic")
        save_cfg({**load_cfg(), "enhance": True})
        return True, "enhanced mic is now default source"
    finally:
        _enh_unlock()


def enhance_deactivate():
    _enh_lock()
    try:
        mods = _enh_modules()
        for m in mods:
            run(["pactl", "unload-module", m])
        if not mods:
            cfg = load_cfg()
            if cfg.get("enhance"):
                save_cfg({**cfg, "enhance": False})
            return True, "not active"
        time.sleep(0.5)
        cfg = load_cfg()
        if cfg.get("enhance"):
            save_cfg({**cfg, "enhance": False})
        sinks, sources = headset_nodes()
        if sources:
            set_default_source(sources[0])
        return True, "off (raw BT mic back)"
    finally:
        _enh_unlock()


def enhance_status():
    return bool(_enh_modules())


def pilot_ensure_unit():
    """Write+enable a small systemd user unit that re-selects profile/default nodes
    whenever the headset (re)connects. Uses bluetoothctl monitor via xm5 pilot."""
    UNIT_DIR.mkdir(parents=True, exist_ok=True)
    unit = """[Unit]
Description=XM5 bluetooth audio mode pilot (keep profile + default nodes correct)

[Service]
ExecStart=%s pilot
Restart=on-failure
RestartSec=2

[Install]
WantedBy=default.target
""" % (sys.executable + " " + str(Path(__file__).resolve()))
    UNIT_FILE.write_text(unit)
    run(["systemctl", "--user", "daemon-reload"])
    run(["systemctl", "--user", "enable", "--now", UNIT_NAME], check=False)


def pilot_loop():
    """Watch headset card/nodes and keep mode + default nodes correct.
    Desired mode is ~/.local/state/xm5/mode: music | mic | auto.
    'auto' never changes profiles; it only repairs broken defaults.
    When the BT mic source is re-created (e.g. after reconnect) and mic-enhance
    is persisted on, reload module-echo-cancel so it binds the fresh source."""
    STATE_DIR.mkdir(parents=True, exist_ok=True)
    mode_file = STATE_DIR / "mode"
    last_state = None
    log("pilot started (pid %d)" % os.getpid())
    while True:
        try:
            cards = headset_cards()
            sinks, sources = headset_nodes()
            state = "|".join(sorted(c["name"] + "=" + active_profile(c) for c in cards)) \
                + "|" + ",".join(sinks) + "|" + ",".join(sources) \
                + "|" + run_ok(["pactl", "get-default-sink"])[1].strip() \
                + "|" + run_ok(["pactl", "get-default-source"])[1].strip() \
                + ("|enh" if enhance_status() else "")
            if state != last_state:
                log("state change: %s" % state)
                last_state = state
                mode = "auto"
                try:
                    mode = mode_file.read_text().strip() or "auto"
                except OSError:
                    pass
                cfg = load_cfg()
                for c in cards:
                    cur = active_profile(c)
                    if cur in ("", "off"):
                        continue
                    want = {"music": MUSIC_PROFILES[0][0],
                            "mic": MIC_PROFILES[0][0]}.get(mode)
                    if want and cur != want:
                        ok, err = set_profile(c["name"], want)
                        log("pilot enforce %s on %s: %s" % (want, c["name"], "ok" if ok else err))
                    if want and mode == "mic" and cfg.get("enhance") and not enhance_status():
                        enhance_activate()
                # mic re-created while enhancement on -> reload to bind fresh source.
                # Only when the capture stream is NOT pointed at the current BT mic
                # (monitors like *.monitor are fine in music mode).
                mic = next((s for s in sources if s.startswith("bluez_input")), None)
                if mic and enhance_status():
                    mic_id = None
                    try:
                        srcs = json.loads(run(["pactl", "-f", "json", "list", "sources"]))
                        for s in srcs:
                            if s.get("name") == mic:
                                mic_id = s.get("index")
                    except (ValueError, subprocess.CalledProcessError):
                        pass
                    bound = None
                    try:
                        outs = json.loads(run(["pactl", "-f", "json", "list", "source-outputs"]))
                        for so in outs:
                            if so.get("properties", {}).get("node.name") == "echo-cancel-capture":
                                bound = so.get("source")
                    except (ValueError, subprocess.CalledProcessError):
                        pass
                    if mic_id and bound is not None and bound != mic_id:
                        log("pilot: capture bound to %s, want %s -> reload enhancement"
                            % (bound, mic_id))
                        enhance_deactivate()
                        time.sleep(0.5)
                        enhance_activate()
                # repair defaults: a headset that has nodes but is not default
                if sinks:
                    if run_ok(["pactl", "get-default-sink"])[1].strip() not in sinks:
                        set_default_sink(sinks[0])
                        log("pilot: default sink -> %s" % sinks[0])
                want_src = "xm5_enhanced_mic" if enhance_status() else (sources[0] if sources else None)
                # in music mode (A2DP) the BT input node is only a stale ghost;
                # prefer the internal mic as default source instead
                if (not want_src or want_src.startswith("bluez_input")) \
                        and any(active_profile(c).startswith("a2dp") for c in cards):
                    try:
                        allsrcs = json.loads(run(["pactl", "-f", "json", "list", "sources"]))
                    except (ValueError, subprocess.CalledProcessError):
                        allsrcs = []
                    alt = next((s["name"] for s in allsrcs
                                if s["name"].startswith("alsa_input")), None)
                    if alt:
                        want_src = alt
                if want_src and run_ok(["pactl", "get-default-source"])[1].strip() != want_src:
                    set_default_source(want_src)
                    log("pilot: default source -> %s" % want_src)
                # headset gone -> tear down enhancement so apps don't hold a dead mic
                if not sources and enhance_status():
                    ok2, msg2 = enhance_deactivate()
                    log("pilot: headset gone, enhance off: %s" % msg2)
            mediats = getattr(pilot_loop, "_mediats", 0.0)
            if time.time() - mediats > 3.0:
                media_cache_write()
                pilot_loop._mediats = time.time()
            # cache the full status for the TUI (file-read instead of proc spawn)
            write_status_cache(sinks, sources)
            time.sleep(2)
        except Exception as e:
            log("pilot error: %r" % e)
            time.sleep(5)


# ---------------------------------------------------------------- media (MPRIS) / calls

MEDIA_CACHE = STATE_DIR / "media.json"

_MPRIS_IFACE = "org.mpris.MediaPlayer2.Player"
_PROP_IFACE = "org.freedesktop.DBus.Properties"

_dbus_mod = None
def _dbus():
    global _dbus_mod
    if _dbus_mod is None:
        import dbus  # system python3 ships it on Ubuntu and Arch (python-dbus)
        _dbus_mod = dbus
    return _dbus_mod


def _dbusb():
    return _dbus().SessionBus()


def _mpris_players(bus):
    names = []
    try:
        names = [str(n) for n in bus.list_names()
                 if str(n).startswith("org.mpris.MediaPlayer2.")]
    except Exception:
        pass
    # stable order: identity-sorted so the active player doesn't jump around
    out = []
    for name in names:
        try:
            obj = bus.get_object(name, "/org/mpris/MediaPlayer2")
            try:
                ident = str(obj.Get(_MPRIS_IFACE, "Identity",
                                    dbus_interface=_PROP_IFACE)
                            or name.rsplit(".", 1)[-1])
            except Exception:
                # some players only expose Identity on the root interface
                ident = name.rsplit(".", 1)[-1]
            out.append((ident.lower(), name, obj))
        except Exception:
            continue
    return [(n, o) for _i, n, o in sorted(out)]


def _str_list(v):
    if v is None:
        return []
    if isinstance(v, str):
        return [v]
    try:
        return [str(x) for x in v]
    except TypeError:
        return []


def mpris_media_status():
    """Snapshot of every MPRIS player + live stream/call detection. Never raises."""
    players = []
    try:
        dbus = _dbus()
        bus = _dbusb()
        for name, obj in _mpris_players(bus):
            try:
                props = dbus.Interface(obj, _PROP_IFACE)
                get = lambda key: props.Get(_MPRIS_IFACE, key)
                md = get("Metadata") or {}
                try:
                    name_get = str(get("Identity") or name.rsplit(".", 1)[-1])
                except Exception:
                    # Identity lives on the root interface in some players
                    name_get = name.rsplit(".", 1)[-1]
                players.append({
                    "name": str(name).rsplit(".", 1)[-1],
                    "identity": name_get,
                    "status": str(get("PlaybackStatus") or "Stopped").lower(),
                    "title": str((md.get("xesam:title") or "")),
                    "artist": ", ".join(_str_list(md.get("xesam:artist"))),
                    "album": str(md.get("xesam:album") or ""),
                    "url": str(md.get("xesam:url") or ""),
                    "length_us": int(md.get("mpris:length") or 0),
                    "position_us": int(obj.Get(_MPRIS_IFACE, "Position",
                                               dbus_interface=_PROP_IFACE) or 0),
                    "can_play": bool(get("CanControl")),
                    "can_next": bool(get("CanGoNext")),
                    "can_prev": bool(get("CanGoPrevious")),
                    "can_seek": bool(get("CanSeek")),
                    "shuffle": bool(get("Shuffle")) if str(get("Shuffle")) != "" else None,
                })
            except Exception:
                continue
    except Exception:
        pass
    if not players:
        players = [{"identity": "no player", "status": "none"}]
    return players

def _player_by_index(idx):
    dbus = _dbus()
    bus = _dbusb()
    pls = _mpris_players(bus)
    if not pls:
        raise RuntimeError("no MPRIS player active")
    i = max(0, min(idx, len(pls) - 1))
    return pls[i][0], pls[i][1], dbus.Interface(pls[i][1], _MPRIS_IFACE)

def media_ctl(idx, action, value=None):
    """Fire MPRIS transport command. Returns short human line."""
    dbus = _dbus()
    name, obj, ctrl = _player_by_index(idx)
    actions = {"play": ctrl.Play, "pause": ctrl.Pause, "playpause": ctrl.PlayPause,
               "stop": ctrl.Stop, "next": ctrl.Next, "prev": ctrl.Previous,
               "next-meta": ctrl.Next, "prev-meta": ctrl.Previous}
    if action == "seek" and value is not None:
        obj.Seek(value, dbus_interface=_MPRIS_IFACE)
    if action == "pos" and value is not None:
        # MPRIS has no absolute-set; emulate by seeking the delta from current
        try:
            cur = int(obj.Get(_MPRIS_IFACE, "Position", dbus_interface=_PROP_IFACE) or 0)
        except Exception:
            cur = 0
        obj.Seek(int(value) - cur, dbus_interface=_MPRIS_IFACE)
        return "position -> %d:%02d" % (value // 60_000_000, value // 1_000_000 % 60)
    actions["seek"] = lambda: obj.Seek(value or 0, dbus_interface=_MPRIS_IFACE)
    fn = actions.get(action)
    if fn is None:
        raise RuntimeError("unbekannte action: %s" % action)
    if callable(fn) and fn not in (obj.Seek,):
        fn()
    return "%s %s" % (name.rsplit(".", 1)[-1], action)


def call_activity():
    """Live capture streams (calls): source-outputs on the BT/echo-cancel mic."""
    calls = []
    try:
        outs = json.loads(run(["pactl", "-f", "json", "list", "source-outputs"]))
        for so in outs:
            p = so.get("properties", {})
            # our own enhancement loopback AND the TUI scope sampler (pw-cat
            # record on the raw BT input) are not real call clients
            if "echo-cancel-capture" in str(p.get("node.name", "")):
                continue
            if str(p.get("application.name", "")) == "pw-cat" \
                    or str(p.get("application.process.binary", "")) == "pw-cat":
                continue
            app = p.get("application.name") or p.get("application.process.binary") \
                or p.get("node.name") or "?"
            if not so.get("mute", False):
                calls.append(str(app))
    except (ValueError, subprocess.CalledProcessError):
        pass
    return sorted(set(calls))


def media_cache_write():
    try:
        data = {"players": mpris_media_status(), "calls": call_activity(),
                "ts": time.time()}
        tmp = MEDIA_CACHE.with_suffix(".tmp")
        tmp.write_text(json.dumps(data, default=str))
        tmp.replace(MEDIA_CACHE)
    except Exception as e:
        log("media cache error: %r" % e)


# ---------------------------------------------------------------- status/json

STATUS_CACHE = STATE_DIR / "status.json"


def write_status_cache(sinks=None, sources=None):
    """Pilot writes the full status here every loop; TUI reads this file
    instead of spawning python (zero-proc idle)."""
    try:
        st = current_card_status(sinks, sources)
        mac = (st.get("device_string") or "").strip()
        if mac and st.get("connected"):
            info = bt_info(mac)
            st["battery"] = info.get("battery")
            st["name"] = info.get("name")
        tmp = STATUS_CACHE.with_suffix(".tmp")
        tmp.write_text(json.dumps(st, default=str))
        tmp.replace(STATUS_CACHE)
    except Exception as e:
        log("status cache error: %r" % e)



def current_card_status(sinks=None, sources=None):
    """Full status dict. Pass pre-fetched (sinks, sources) names to avoid
    duplicate pactl spawns when the caller already has them."""
    cards = headset_cards()
    if sinks is None or sources is None:
        sinks, sources = headset_nodes()
    if not cards:
        pilot_on = pilot_active()
        st = anc_state()
        st.update({
            "connected": False,
            "mode": "off",
            "profile": "off",
            "pilot": pilot_on,
            "battery": None,
            "name": None,
            "enhance": enhance_status(),
        })
        return st
    c = cards[0]
    props = c["properties"]
    prof = active_profile(c)
    out = {
        "connected": True,
        "card": c["name"],
        "profile": prof,
        "mode": "music" if is_music_profile(prof) else ("off" if prof in ("", "off") else "mic"),
        "profiles": card_profiles(c),
        "battery": None,
        "device_string": props.get("device.string", ""),
        "sinks": sinks,
        "sources": sources,
    }
    try:
        sdata = json.loads(run(["pactl", "-f", "json", "list", "sinks"]))
        default_sink = run(["pactl", "get-default-sink"]).strip()
        for s in sdata:
            if s["name"] in sinks:
                out["sink"] = s["name"]
                out["volume"], out["muted"] = sink_volume(s["name"])
                out["codec"] = s.get("properties", {}).get("api.bluez5.codec")
    except (ValueError, subprocess.CalledProcessError):
        pass
    out["default_sink"] = run_ok(["pactl", "get-default-sink"])[1].strip()
    out["default_source"] = run_ok(["pactl", "get-default-source"])[1].strip()
    out["enhance"] = enhance_status()
    out["anc"] = anc_state().get("anc")
    out["anc_level"] = anc_state().get("level")
    out["pilot"] = pilot_active()
    out["battery"] = None
    out["name"] = None
    return out


_PILOT_CACHE = (0.0, False)


def pilot_active() -> bool:
    """systemctl is-active, cached for 10s (TUI polls every 250ms)."""
    global _PILOT_CACHE
    now = time.monotonic()
    if now - _PILOT_CACHE[0] > 10.0:
        v = run_ok(["systemctl", "--user", "is-active",
                    "xm5-pilot.service"])[1].strip() == "active"
        _PILOT_CACHE = (now, v)
    return _PILOT_CACHE[1]


def cmd_status():
    st = current_card_status()
    print(json.dumps(st, indent=2, default=str))


def cmd_device():
    """Cheap battery/name fetch for TUI background thread (called rarely)."""
    st = current_card_status()
    mac = (st.get("device_string") or "").strip()
    info = bt_info(mac) if mac else {}
    print(json.dumps({"battery": info.get("battery"),
                      "name": info.get("name"),
                      "connected": st.get("connected", False)},
                     indent=2, default=str))

# ---------------------------------------------------------------- CLI


def cmd_volume(args):
    st = current_card_status()
    sink = st.get("sink")
    if not sink:
        die("no headset sink connected")
    if args and args[0] == "set":
        change_volume(sink, target=int(args[1]))
    elif args and args[0].startswith("set"):
        change_volume(sink, target=int(args[0][3:]))
    elif args and args[0] == "+":
        change_volume(sink, delta=load_cfg()["step"])
    elif args and args[0] == "-":
        change_volume(sink, delta=-load_cfg()["step"])
    elif args and args[0] == "max":
        change_volume(sink, target=100)
    elif args and args[0].lstrip("+-").isdigit():
        cur, _ = sink_volume(sink)
        change_volume(sink, target=(cur or 0) + int(args[0]))
    else:
        die("usage: xm5 volume [+|-|N|max|set N]")
    vol, mute = sink_volume(sink)
    print("volume %s%% mute=%s" % (vol, "yes" if mute else "no"))


def cmd_mode(args):
    if not args:
        die("usage: xm5 mode <music|mic|toggle|auto>")
    m = args[0]
    if m == "auto":
        STATE_DIR.mkdir(parents=True, exist_ok=True)
        (STATE_DIR / "mode").write_text("auto\n")
        pilot_ensure_unit()
        print("auto mode: pilot unit enabled, defaults auto-repaired on reconnect")
        return
    ok, msg = goto_mode(m)
    print(msg)
    play_sound({True: m if m in ("music", "mic") else "music",
                False: "error"}.get(ok, "error"))
    if not ok:
        sys.exit(1)


def cmd_enhance(args):
    what = args[0] if args else "status"
    if what == "on":
        ok, msg = enhance_activate()
        print(msg)
        sys.exit(0 if ok else 1)
    if what == "off":
        ok, msg = enhance_deactivate()
        print(msg)
        sys.exit(0 if ok else 1)
    if what == "toggle":
        ok, msg = enhance_deactivate() if enhance_status() else enhance_activate()
        print(msg)
        sys.exit(0 if ok else 1)
    print("enhanced mic %s" % ("active" if enhance_status() else "inactive"))


def cmd_battery():
    st = current_card_status()
    print("battery %s" % (st.get("battery") if st.get("battery") is not None else "unknown"))


def cmd_repair():
    ok, msg = repair()
    play_sound("connected" if ok else "error")
    print(msg)
    sys.exit(0 if ok else 1)


def cmd_discover():
    for d in bt_device_list():
        info = bt_info(d["mac"])
        print("%-17s %-30s %s" % (d["mac"], d["name"],
              "connected" if info["connected"] else "paired"))


def main():
    argv = sys.argv[1:]
    if not argv:
        # interactive TUI lives in the Rust binary; keep python engine pure-CLI
        import shutil
        tui_bin = shutil.which("xm5-tui") or shutil.which("xm5-tui-bin")
        if tui_bin:
            os.execvp(tui_bin, [tui_bin])
        die("TUI binary not found - run install.py (xm5-tui)")

    cmd = argv[0]
    rest = argv[1:]
    if cmd in ("status", "--json"):
        cmd_status()
    elif cmd == "device":
        cmd_device()
    elif cmd == "volume":
        cmd_volume(rest)
    elif cmd == "mode":
        cmd_mode(rest)
    elif cmd.startswith("vol"):
        cmd_volume(rest if not cmd.startswith("vol+") else ["+"])
    elif cmd in ("mic-enhance", "enhance"):
        cmd_enhance(rest)
    elif cmd == "anc":
        cmd_anc(rest)
    elif cmd == "battery":
        cmd_battery()
    elif cmd == "repair":
        cmd_repair()
    elif cmd == "discover":
        cmd_discover()
    elif cmd == "media":
        data = {"players": mpris_media_status(), "calls": call_activity()}
        print(json.dumps(data, indent=2, default=str))
    elif cmd == "media-ctl":
        try:
            idx = int(rest[0])
            action = rest[1]
            value = int(rest[2]) if len(rest) > 2 else None
            print(media_ctl(idx, action, value))
        except (IndexError, ValueError):
            die("usage: xm5 media-ctl <player-idx> <play|pause|playpause|stop|next|prev|seek us|pos us>")
    elif cmd == "pilot":
        pilot_loop()
    elif cmd in ("install", "service"):
        pilot_ensure_unit()
        print("installed and started " + UNIT_NAME)
    elif cmd in ("help", "--help", "-h"):
        print(__doc__)
    else:
        die("unknown command: %s (see xm5 help)" % cmd)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        sys.exit(130)
    except BrokenPipeError:
        sys.exit(0)
