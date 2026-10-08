#!/usr/bin/env python3
"""installer for xm5: TUI launcher symlinks + Hyprland shortcuts (Omarchy) + service."""
import shutil
import subprocess
import sys
import tarfile
from pathlib import Path

HOME = Path.home()
SCRIPT = Path(__file__).resolve().parent / "xm5.py"
ROOT = Path(__file__).resolve().parent
BIN_DIR = HOME / ".local/bin"
BINDINGS = HOME / ".config/hypr/bindings.lua"
CACHE_DIR = HOME / ".cache/xm5"

# TUI provisioning sources, in order of preference (see tui_binary()):
#   1. target/release/xm5-tui     locally built already
#   2. prebuilt/xm5-tui           static-pie musl build committed with the repo,
#                                 runs on any glibc/musl distro, no download
#   3. cargo + vendor.tar.xz      offline build (crates snapshot, no network)
#   4. cargo + network            last resort on a networked machine
# Without 1-3 the old code failed hard on machines that can reach GitHub but
# not crates.io (proxy/offline CI/restricted DNS) - see README "Offline builds".


def sh(cmd):
    return subprocess.run(cmd, shell=True, capture_output=True, text=True)


def build_offline(tui_src, cache: bool):
    """Unpack the vendored crates snapshot and build offline. Returns the path
    to the release binary, or None. Never touches the network."""
    tarball = tui_src / "vendor.tar.xz"
    if not tarball.exists() or not shutil.which("cargo"):
        return None
    vendor = (CACHE_DIR if cache else tui_src / ".vendor") / "vendor"
    try:
        if not (vendor / "ratatui").is_dir():
            print("  unpacking vendored crates -> %s" % vendor)
            vendor.parent.mkdir(parents=True, exist_ok=True)
            with tarfile.open(tarball, "r:xz") as tf:
                tf.extractall(vendor.parent, filter="data")
    except (tarfile.TarError, OSError) as e:
        print("  warning: cannot unpack %s: %s" % (tarball.name, e), file=sys.stderr)
        return None
    args = [
        "cargo", "build", "--release", "--locked", "--offline",
        "--config", 'source.crates-io.replace-with="vendored-sources"',
        "--config", 'source.vendored-sources.directory="%s"' % vendor,
    ]
    r = subprocess.run(args, cwd=str(tui_src), capture_output=True, text=True)
    if r.returncode != 0:
        print("  offline build failed: %s" % (r.stderr.strip()[-400:] or "cargo error"),
              file=sys.stderr)
        return None
    return tui_src / "target/release/xm5-tui"


def tui_binary(tui_src):
    """Provision a runnable xm5-tui, preferring sources that need no network."""
    local = tui_src / "target/release/xm5-tui"
    if local.exists():
        return local
    prebuilt = tui_src / "prebuilt/xm5-tui"
    if prebuilt.exists() and shutil.which("cargo") is None:
        print("using bundled prebuilt xm5-tui (static, no Rust toolchain needed)")
        return prebuilt
    if shutil.which("cargo"):
        print("building xm5-tui (cargo, offline via vendored crates)…")
        built = build_offline(tui_src, cache=True)
        if built:
            return built
        print("  falling back to a networked cargo build…")
        r = subprocess.run(["cargo", "build", "--release"], cwd=str(tui_src),
                           capture_output=True, text=True)
        if local.exists():
            return local
        print("  cargo build failed: %s" % (r.stderr.strip()[-400:] or "cargo error"),
              file=sys.stderr)
        if prebuilt.exists():
            print("using bundled prebuilt xm5-tui instead", file=sys.stderr)
            return prebuilt
        return None
    if prebuilt.exists():
        return prebuilt
    return None


def main():
    # hard dependencies: pactl (pipewire-pulse or pulseaudio) + bluetoothctl
    missing = [d for d in ("pactl", "bluetoothctl")
               if not shutil.which(d)]
    if missing:
        print("missing packages: %s" % ", ".join(missing), file=sys.stderr)
        print("  Arch:     sudo pacman -S pipewire-pulse bluez-utils", file=sys.stderr)
        print("  Ubuntu:   sudo apt install pipewire-pulse bluez", file=sys.stderr)
        return 1
    if not SCRIPT.exists():
        print("xm5.py not found next to install.py", file=sys.stderr)
        return 1
    # seed ANC state (XM5 ships with ANC on)
    state_dir = HOME / ".local/state/xm5"
    state_dir.mkdir(parents=True, exist_ok=True)
    anc_state = state_dir / "anc"
    if not anc_state.exists():
        anc_state.write_text("on 15\n")

    BIN_DIR.mkdir(parents=True, exist_ok=True)
    # real TUI binary (rust) next to wrapper; provision it (offline first)
    tui_src = ROOT / "xm5-tui"
    tui_bin = tui_binary(tui_src)
    dest = BIN_DIR / "xm5-tui-bin"
    if tui_bin is not None and tui_bin.exists():
        # unlink first: replacing a RUNNING binary inode is fine, write_bytes
        # on the running file itself would fail with ETXTBSY
        if dest.exists():
            dest.unlink()
        dest.write_bytes(tui_bin.read_bytes())
        dest.chmod(0o755)
        launcher = BIN_DIR / "xm5-tui"
        launcher.write_text(
            '#!/bin/sh\n# xm5-tui wrapper: pins the python engine file to avoid PATH loops\n'
            'XM5_ENGINE="%s"\nexport XM5_ENGINE\nexec %s "$@"\n'
            % (Path(__file__).resolve().parent / "xm5.py", dest))
        launcher.chmod(0o755)
        print("installed xm5-tui binary")
    else:
        print("WARNING: xm5-tui binary missing - install rust (https://rustup.rs)"
              " and re-run install.py")

    # command shortcuts in ~/.local/bin
    for name, args in {
        "xm5": [],
        "xm5-music": ["mode", "music"],
        "xm5-mic": ["mode", "mic"],
        "xm5-toggle": ["mode", "toggle"],
        "xm5-volup": ["volume", "+"],
        "xm5-voldown": ["volume", "-"],
        "xm5-repair": ["repair"],
    }.items():
        target = BIN_DIR / name
        if args:
            target.write_text("#!/bin/sh\nexec %s %s \"$@\"\n" % (SCRIPT, " ".join(args)))
            target.chmod(0o755)
        else:
            tmp = BIN_DIR / (name + ".tmp-link")
            if tmp.exists() or target.is_symlink():
                target.unlink(missing_ok=True)
            tmp.symlink_to(SCRIPT)
            if target.exists():
                target.unlink()
            tmp.rename(target)

    print("symlinked xm5 ->", SCRIPT)

    # UI sounds live next to the engine (played via paplay)
    sounds_src = Path(__file__).resolve().parent / "sounds"
    if sounds_src.is_dir():
        sounds_dst = SCRIPT.parent / "sounds"
        sounds_dst.mkdir(parents=True, exist_ok=True)
        for wav in sounds_src.glob("*.wav"):
            (sounds_dst / wav.name).write_bytes(wav.read_bytes())
        print("installed %d UI sounds" % len(list(sounds_dst.glob('*.wav'))))

    # systemd user service (pilot)
    r = sh("%s install" % SCRIPT)
    print(r.stdout.strip() or r.stderr.strip())

    # hyprland bindings (Omarchy). Skip silently on non-Hyprland systems (Ubuntu GNOME).
    if not BINDINGS.exists():
        print("no %s, skipping hyprland bindings" % BINDINGS)
        return
    text = BINDINGS.read_text()
    if "XM5 headset TUI" in text:
        print("hyprland bindings already present")
        return
    # remove legacy blocks from earlier installer runs (whole comment line + binds)
    text = "\n".join(l for l in text.splitlines()
                     if "xm5" not in l.lower() and "XM5" not in l) + "\n"
    block = """
-- XM5 headset: mode toggle + volume (added by xm5 installer)
-- NOTE: plain SUPER+ALT+UP/DOWN are used by Omarchy's tiling group-move, so
-- volume uses SUPER+ALT+SHIFT instead.
o.bind("SUPER + ALT + M", "XM5 headset TUI + toggle", "omarchy-launch-or-focus-tui xm5")
o.bind("SUPER + ALT + SHIFT + UP", "XM5 volume up", "xm5-volup", { repeating = true })
o.bind("SUPER + ALT + SHIFT + DOWN", "XM5 volume down", "xm5-voldown", { repeating = true })
o.bind("SUPER + ALT + SHIFT + R", "XM5 repair bluetooth audio", "xm5-repair")
"""
    BINDINGS.write_text(text + block)
    print("added hyprland bindings (SUPER+ALT+M tui, SUPER+ALT+UP/DOWN volume)")
    r = sh("hyprctl reload; hyprctl configerrors")
    print((r.stdout or "").strip())


if __name__ == "__main__":
    main()