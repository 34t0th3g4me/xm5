// xm5-tui — Nothing-X-style dashboard for Sony WH-1000XM5 on Linux.
// Responsive dashboard: device · noise-cancel pods · volume · quick actions ·
// full-width mic scope widget. Grey tiles, rounded borders, filled white pill
// when a pod is active, breathing status dot, accent staggers.
// Mouse: click pods/buttons, click+drag volume, scroll volume.
// Latency: optimistic pod/level flips; engine calls run in background threads;
// mic scope reads a pw-cat u8 stream in its own thread (peaks → rolling bars).

use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Paragraph},
    Frame, Terminal,
};
use serde::Deserialize;
use std::{
    io::{self, Read, Stdout},
    process::{Child, ChildStdout, Command, Stdio},
    sync::{mpsc, atomic::{AtomicBool, Ordering}},
    thread,
    time::{Duration, Instant},
};
use parking_lot::Mutex;

// ---------------------------------------------------------------- status model

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
struct Status {
    connected: bool,
    profile: String,
    mode: String,
    battery: Option<u8>,
    volume: Option<u8>,
    muted: Option<bool>,
    codec: Option<String>,
    pilot: bool,
    name: Option<String>,
    #[serde(default)]
    anc: Option<String>,
    #[serde(default)]
    anc_level: Option<u8>,
    #[serde(default)]
    enhance: bool,
}

fn engine_exe() -> String {
    std::env::var("XM5_ENGINE").unwrap_or_else(|_| "xm5".to_string())
}

fn fetch_status() -> Status {
    // fast path: pilot's status cache (zero procs)
    if let Ok(meta) = std::fs::metadata(status_cache_path()) {
        if let Ok(mtime) = meta.modified() {
            if mtime.elapsed().map(|d| d < Duration::from_millis(2600)).unwrap_or(false) {
                if let Ok(data) = std::fs::read(status_cache_path()) {
                    if let Ok(s) = serde_json::from_slice::<Status>(&data) {
                        return s;
                    }
                }
            }
        }
    }
    // slow path: engine call (pilot dead or cache stale)
    let out = Command::new(engine_exe())
        .arg("status")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();
    out.ok()
        .and_then(|o| serde_json::from_slice::<Status>(&o.stdout).ok())
        .unwrap_or_default()
}
fn status_cache_path() -> std::path::PathBuf {
    match std::env::var("XM5_STATE_DIR") {
        Ok(dir) => std::path::PathBuf::from(dir).join("status.json"),
        Err(_) => {
            let mut p = std::env::var("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| std::path::PathBuf::from("/tmp"));
            p.push(".local/state/xm5");
            p.push("status.json");
            p
        }
    }
}

fn status_cache_mtime() -> Option<std::time::SystemTime> {
    std::fs::metadata(status_cache_path()).ok()?.modified().ok()
}

fn engine_run(args: &[&str]) -> String {
    Command::new(engine_exe())
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map(|o| {
            let s = String::from_utf8_lossy(&o.stdout);
            let e = String::from_utf8_lossy(&o.stderr);
            format!("{}{}", s, e).trim().to_string()
        })
        .unwrap_or_else(|e| format!("engine error: {e}"))
}

// ---------------------------------------------------------------- theme

const BG: Color = Color::Rgb(10, 10, 10);
const TILE: Color = Color::Rgb(24, 24, 24);
const TILE2: Color = Color::Rgb(30, 30, 30); // inset chip bg
const TILE_BORDER: Color = Color::Rgb(64, 64, 64);
const FG: Color = Color::Rgb(242, 242, 242);
const DIM: Color = Color::Rgb(128, 128, 128);
const FAINT: Color = Color::Rgb(58, 58, 58);
const GREEN: Color = Color::Rgb(88, 220, 130);
const RED: Color = Color::Rgb(232, 92, 92);
const FLASH_COL: Color = Color::Rgb(125, 211, 252); // poppy electric-blue flash
const ORANGE: Color = Color::Rgb(255, 179, 71);
const ACCENT: Color = Color::Rgb(125, 211, 252); // electric blue for active elements
const VOL_HI: Color = Color::Rgb(242, 242, 242); // volume gradient head
const VOL_MID: Color = Color::Rgb(160, 160, 160);

// ---------------------------------------------------------------- app state

enum Msg {
    Status(Status),
    Done(#[allow(dead_code)] String),
    Mic { samples: Vec<f32>, live: bool, src: String, db: f32 },
    Media(MediaInfo),
}

static ACTION_TX: Mutex<Option<mpsc::Sender<Msg>>> = Mutex::new(None);

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct MediaPlayer {
    #[serde(default)]
    identity: String,
    #[serde(default)]
    status: String, // playing | paused | stopped | none
    #[serde(default)]
    title: String,
    #[serde(default)]
    artist: String,
    #[serde(default)]
    album: String,
    #[serde(default)]
    length_us: i64,
    #[serde(default)]
    position_us: i64,
}

#[derive(Debug, Clone, Default)]
struct MediaInfo {
    player: Option<MediaPlayer>, // preferred (playing) or first player
    calls: Vec<String>,          // apps capturing the mic right now (Teams etc.)
}

fn media_cache_path() -> std::path::PathBuf {
    match std::env::var("XM5_STATE_DIR") {
        Ok(dir) => std::path::PathBuf::from(dir).join("media.json"),
        Err(_) => {
            let mut p = std::env::var("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| std::path::PathBuf::from("/tmp"));
            p.push(".local/state/xm5");
            p.push("media.json");
            p
        }
    }
}

fn fetch_media() -> MediaInfo {
    let mut info = MediaInfo::default();
    if let Ok(data) = std::fs::read(media_cache_path()) {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&data) {
            let players: Vec<MediaPlayer> = v
                .get("players")
                .and_then(|p| serde_json::from_value(p.clone()).ok())
                .unwrap_or_default();
            // prefer a playing player; else the first real one (status != none)
            info.player = players
                .iter()
                .find(|p| p.status == "playing")
                .or_else(|| players.iter().find(|p| p.status != "none"))
                .cloned();
            if let Some(c) = v.get("calls").and_then(|c| c.as_array()) {
                info.calls = c
                    .iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect();
            }
        }
    }
    info
}

/// position we render: cache pos + monotonic drift since cache write
fn live_position(p: &MediaPlayer, age: std::time::Duration) -> i64 {
    if p.status != "playing" || p.length_us <= 0 {
        return p.position_us;
    }
    let drift = age.as_micros() as i64;
    (p.position_us + drift).min(p.length_us)
}

fn media_cache_mtime() -> Option<std::time::SystemTime> {
    std::fs::metadata(media_cache_path()).ok()?.modified().ok()
}

struct App {
    status: Status,
    toast: Option<Toast>,
    media: MediaInfo,
    busy: bool,
    quit: bool,
    help: bool,
    pod_rects: [Rect; 3],
    vol_rect: Rect,
    btn_rects: Vec<(Rect, &'static str)>,
    hover: Option<String>,
    btn_flash: Option<(&'static str, Instant)>,
    vol_hover_x: Option<u16>,
    vol_bump: i8,
    vol_bump_at: Option<Instant>,
    anc_pending: Option<String>,
    anc_pending_at: Option<Instant>,
    mic: MicView,
    frame: u64,
    dirty: bool,
    pod_flash: Option<(usize, Instant)>,
    start_at: Instant,
}

#[derive(Debug, Clone, Default)]
struct MicView {
    samples: Vec<f32>,
    live: bool,
    src: String,
    db: f32,
}

struct Toast {
    msg: String,
    at: Instant,
    spinning: bool,
}

impl App {
    fn new() -> Self {
        App {
            status: Status::default(),
            media: MediaInfo::default(),
            toast: None,
            busy: false,
            quit: false,
            help: false,
            pod_rects: [Rect::default(); 3],
            vol_rect: Rect::default(),
            btn_rects: Vec::new(),
            hover: None,
            btn_flash: None,
            vol_bump: 0,
            vol_hover_x: None,
            vol_bump_at: None,
            anc_pending: None,
            anc_pending_at: None,
            mic: MicView::default(),
            frame: 0,
            dirty: true,
            pod_flash: None,
            start_at: Instant::now(),
        }
    }
}

// ---------------------------------------------------------------- mic scope

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static MIC_CHILD: Mutex<Option<Child>> = Mutex::new(None);

/// XM5 mic source: bluez_input* from the pilot cache; only while connected.
/// XM5_MIC_ANY=1 falls back to the default source (test hook for any mic).
fn detect_mic_source() -> Option<String> {
    if std::env::var("XM5_MIC_ANY").map(|v| v == "1").unwrap_or(false) {
        if let Ok(o) = Command::new("pactl").arg("get-default-source").output() {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.starts_with("alsa_input") || s.starts_with("bluez_input") {
                return Some(s);
            }
        }
    }
    if let Ok(data) = std::fs::read(status_cache_path()) {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&data) {
            let any = std::env::var("XM5_MIC_ANY").map(|v| v == "1").unwrap_or(false);
            if v.get("connected").and_then(|c| c.as_bool()) == Some(false) && !any {
                return None;
            }
            if let Some(srcs) = v.get("sources").and_then(|s| s.as_array()) {
                for s in srcs {
                    if let Some(name) = s.as_str() {
                        if name.starts_with("bluez_input") {
                            return Some(name.to_string());
                        }
                    }
                }
            }
        }
    }
    None
}

fn spawn_pwcat(src: &str) -> Option<(Child, ChildStdout)> {
    let mut cmd = Command::new("pw-cat");
    cmd.arg("--record")
        .arg("--raw")
        .arg("--target").arg(src)
        .args(["--rate", "4000", "--channels", "1", "--format", "u8", "--latency", "16ms"])
        .arg("-")
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut c = cmd.spawn().ok()?;
    let out = c.stdout.take()?;
    Some((c, out))
}

fn send_idle(tx: &mpsc::Sender<Msg>) {
    let _ = tx.send(Msg::Mic { samples: Vec::new(), live: false, src: String::new(), db: 0.0 });
}

fn kill_mic() {
    if let Some(mut c) = MIC_CHILD.lock().take() {
        let _ = c.kill();
        let _ = c.wait();
    }
}

fn mic_thread(tx: mpsc::Sender<Msg>) {
    let mut buf = [0u8; 512];
    let mut samples: Vec<f32> = Vec::with_capacity(240);
    let mut target: Option<String> = None;
    let mut stream: Option<ChildStdout> = None;
    let mut chunk_no: u64 = 0;
    let mut peak_gain: f32 = 0.2;
    let mut last_probe = Instant::now() - Duration::from_secs(5);
    loop {
        if SHUTDOWN.load(Ordering::Relaxed) {
            kill_mic();
            return;
        }
        if target.is_none() && last_probe.elapsed() > Duration::from_secs(2) {
            last_probe = Instant::now();
            match detect_mic_source() {
                Some(src) => match spawn_pwcat(&src) {
                    Some((c, out)) => {
                        *MIC_CHILD.lock() = Some(c);
                        stream = Some(out);
                        target = Some(src.clone());
                    }
                    None => last_probe -= Duration::from_secs(6), // retry later
                },
                None => {}
            }
        }
        if target.is_none() {
            thread::sleep(Duration::from_millis(120));
            continue;
        }
        // read one PCM chunk → peak (pw-cat delivers ~64ms chunks)
        let n = match stream.as_mut().map(|s| s.read(&mut buf)) {
            Some(Ok(n)) => n,
            _ => 0,
        };
        if n == 0 {
            // process died (headset gone / profile left HFP)
            kill_mic();
            stream = None;
            target = None;
            samples.clear();
            samples.shrink_to_fit();
            send_idle(&tx);
            thread::sleep(Duration::from_millis(200));
            continue;
        }
        let peak = buf[..n]
            .iter()
            .map(|b| (*b as i16 - 128) as f32 / 127.0)
            .fold(0.0f32, |a, v| a.max(v.abs()));
        // auto-gain: track a slow-falling max so quiet mics still show a
        // lively wave (normalized peak 0..1)
        let ref_gain = peak_gain.max(peak).max(0.06) * 0.997;
        if std::env::var("XM5_DEBUG_MIC").map(|v| v == "1").unwrap_or(false) {
            let _ = std::fs::write(
                std::env::var("XM5_STATE_DIR").unwrap_or_else(|_| "/tmp/xm5dbg".into())
                    + "/micdbg.json",
                format!(r#"{{"peak":{},"gain":{},"norm":{},"n":{}}}"#,
                    peak, ref_gain, (peak / ref_gain), samples.len()),
            );
        }
        peak_gain = ref_gain;
        samples.push((peak / ref_gain).clamp(0.0, 1.0));
        if samples.len() > 220 {
            let over = samples.len() - 220;
            samples.drain(..over);
        }
        chunk_no += 1;
        if chunk_no % 2 == 0 {
            // ~8 fps scope refresh; dB of normalized signal vs full scale
            let db = 20.0 * (peak / peak_gain.max(0.06)).max(0.001).log10();
            let _ = tx.send(Msg::Mic {
                samples: samples.clone(),
                live: true,
                src: target.clone().unwrap_or_default(),
                db,
            });
        }
    }
}

// ---------------------------------------------------------------- widgets

fn tile(f: &mut Frame, area: Rect, border: Option<(&str, Color)>, accent_border: Option<Color>) -> Rect {
    let mut b = Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(accent_border.unwrap_or(TILE_BORDER)).bg(BG))
        .style(Style::default().bg(TILE));
    if let Some((t, col)) = border {
        b = b.title(Span::styled(
            format!(" {} ", t),
            Style::default().fg(col).add_modifier(Modifier::BOLD),
        ));
    }
    let inner = b.inner(area);
    f.render_widget(b, area);
    inner
}

/// ring pod: ╭─────╮ │ icon │ ╰─────╯ — mid row same width as ring; when on,
/// the whole inner pill is filled with FG (icon inverted); label row below.
fn pod(f: &mut Frame, area: Rect, label: &str, on: bool, hover: bool, flash: bool) {
    let rw = ((area.width / 2) as u16).clamp(7, 11).max(7);
    let gx = area.x + area.width.saturating_sub(rw) / 2;
    let (ring_col, fill_col, icon_col) = if on {
        (FG, FG, BG)
    } else if flash {
        (FLASH_COL, TILE, FLASH_COL)
    } else if hover {
        (FG, TILE, FG)
    } else {
        (DIM, TILE, DIM)
    };
    let icon = POD_ICONS[pod_idx(label)];
    let inner = (rw - 2) as usize;
    let pad = inner.saturating_sub(1) / 2;
    let ring_style = Style::default().fg(ring_col);
    let top = Line::from(Span::styled(
        format!("╭{}╮", "─".repeat(rw.saturating_sub(2) as usize)),
        ring_style,
    ));
    let mid = Line::from(vec![
        Span::styled("│", ring_style),
        Span::styled(" ".repeat(pad), Style::default().bg(fill_col)),
        Span::styled(icon, Style::default().fg(icon_col).bg(fill_col).add_modifier(Modifier::BOLD)),
        Span::styled(" ".repeat(inner - 1 - pad), Style::default().bg(fill_col)),
        Span::styled("│", ring_style),
    ]);
    let bot = Line::from(Span::styled(
        format!("╰{}╯", "─".repeat(rw.saturating_sub(2) as usize)),
        ring_style,
    ));
    f.render_widget(top, Rect { x: gx, y: area.y, width: rw, height: 1 });
    f.render_widget(mid, Rect { x: gx, y: area.y + 1, width: rw, height: 1 });
    f.render_widget(bot, Rect { x: gx, y: area.y + 2, width: rw, height: 1 });
    let label_style = if on {
        Style::default().fg(FG).add_modifier(Modifier::BOLD)
    } else if flash {
        Style::default().fg(FLASH_COL)
    } else if hover {
        Style::default().fg(FG)
    } else {
        Style::default().fg(DIM)
    };
    f.render_widget(
        Paragraph::new(Span::styled(label, label_style))
            .alignment(ratatui::layout::Alignment::Center),
        Rect { x: area.x, y: area.y + 3, width: area.width, height: 1 },
    );
}

/// compact inline pod for tiny tiles: 󰖔 noise cancel
fn pod_inline(f: &mut Frame, area: Rect, label: &str, icon: &str, on: bool, hover: bool, flash: bool) {
    let col = if on {
        FG
    } else if flash {
        FLASH_COL
    } else if hover {
        FG
    } else {
        DIM
    };
    let mut l = Line::from(vec![
        Span::styled(" ", Style::default()),
        Span::styled(icon, Style::default().fg(col).add_modifier(Modifier::BOLD)),
        Span::styled(" ", Style::default()),
        Span::styled(
            label,
            if on {
                Style::default().fg(FG).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(col)
            },
        ),
    ]);
    l.spans.insert(0, Span::raw(""));
    f.render_widget(l, area);
}

/// quick-action state model (single source shared by chips + footer)
struct QAct {
    id: &'static str,     // hover/flash id ("music", "mic", …)
    key: &'static str,    // key badge letter ("" for space-key auto)
    icon: &'static str,
    label: &'static str,
    armed: bool,          // true when the current state IS this action's result
}

fn quick_actions(app: &App) -> [QAct; 6] {
    let mode = app.status.mode.as_str();
    [
        QAct { id: "music", key: "M", icon: "󰝚", label: "Music",
               armed: mode == "music" },
        QAct { id: "mic", key: "V", icon: "󰋎", label: "Mic",
               armed: mode == "mic" },
        QAct { id: "toggle", key: "T", icon: "󰓡", label: "Toggle",
               armed: false },
        QAct { id: "dsp", key: "E", icon: "󰘮", label: "DSP",
               armed: app.status.enhance },
        QAct { id: "repair", key: "R", icon: "󰒓", label: "Repair",
               armed: false },
        QAct { id: "auto", key: "A", icon: "󰁨", label: "Auto",
               armed: app.status.pilot },
    ]
}

fn quick_engine_args(id: &str) -> Option<Vec<&'static str>> {
    Some(match id {
        "music" => vec!["mode", "music"],
        "mic" => vec!["mode", "mic"],
        "toggle" => vec!["mode", "toggle"],
        "dsp" => vec!["mic-enhance", "toggle"],
        "repair" => vec!["repair"],
        "auto" => vec!["mode", "auto"],
        "m_play" => vec!["media-ctl", "0", "playpause"],
        "m_stop" => vec!["media-ctl", "0", "stop"],
        "m_next" => vec!["media-ctl", "0", "next"],
        "m_prev" => vec!["media-ctl", "0", "prev"],
        "m_back" => vec!["media-ctl", "0", "seek", "-10000000"],
        "m_fwd" => vec!["media-ctl", "0", "seek", "10000000"],
        _ => return None,
    })
}

/// pill button, 2 rows wide layout:
///  ╭────╮
///  │K ic│     key badge in top-left, icon+label on a soft pill bg;
///  ╰────╯     armed → filled FG pill with BG text (matches ANC pods),
///             hover → border + text FG, flash → ACCENT border burst
fn quick_pill(f: &mut Frame, area: Rect, a: &QAct, hover: bool, flash: bool) {
    if area.width < 5 {
        return;
    }
    let w = area.width;
    let (brd, txt, fill, keybg, keyfg) = if a.armed {
        (FG, BG, FG, BG, FG)
    } else if flash {
        (FLASH_COL, FLASH_COL, TILE, FLASH_COL, BG)
    } else if hover {
        (FG, FG, TILE2, FG, BG)
    } else {
        (TILE_BORDER, DIM, TILE, TILE2, DIM)
    };
    let top = Line::from(Span::styled(
        format!("╭{}╮", "─".repeat(w.saturating_sub(2) as usize)),
        Style::default().fg(brd),
    ));
    let bot = Line::from(Span::styled(
        format!("╰{}╯", "─".repeat(w.saturating_sub(2) as usize)),
        Style::default().fg(brd),
    ));
    // mid row: key badge + icon + label, bg = fill
    let key_badge = if a.key.is_empty() { " ".to_string() } else { a.key.to_string() };
    let mid = Line::from(vec![
        Span::styled("│", Style::default().fg(brd)),
        Span::styled(" ", Style::default().bg(fill)),
        Span::styled(
            key_badge,
            Style::default().fg(keyfg).bg(if a.key.is_empty() { fill } else { keybg })
                .add_modifier(if !a.key.is_empty() { Modifier::BOLD } else { Modifier::empty() }),
        ),
        Span::styled(" ", Style::default().bg(fill)),
        Span::styled(
            a.icon,
            Style::default().fg(if a.armed { BG } else { txt }).bg(fill)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ", Style::default().bg(fill)),
        Span::styled(
            a.label,
            Style::default().fg(if a.armed { BG } else { txt }).bg(fill)
                .add_modifier(if a.armed { Modifier::BOLD } else { Modifier::empty() }),
        ),
        Span::styled(" ", Style::default().bg(fill)),
        Span::styled("│", Style::default().fg(brd)),
    ]);
    f.render_widget(top, Rect { x: area.x, y: area.y, width: w, height: 1 });
    f.render_widget(mid, Rect { x: area.x, y: area.y + 1, width: w, height: 1 });
    if area.height >= 3 {
        f.render_widget(bot, Rect { x: area.x, y: area.y + 2, width: w, height: 1 });
    }
}

/// quick tile body: status line + pill/chip grid; registers btn_rects for
/// mouse handling, returns Nothing-X-style status string for the footer
fn quick_tile(f: &mut Frame, inner: Rect, app: &mut App) -> String {
    let acts = quick_actions(app);
    let flash_id = app
        .btn_flash
        .filter(|(_, at)| at.elapsed() < Duration::from_millis(420))
        .map(|(id, _)| id);
    let connected = app.status.connected;
    let next_mode = if app.status.mode == "mic" { "music" } else { "mic" };
    // adaptive layout: pills (2 rows) when height allows, chips otherwise;
    // 3 per row at wide tiles, 2 at mid, stacked single column in tiny tiles
    let wi = inner.width.saturating_sub(2) as usize;
    let per_row = if wi >= 44 { 3 } else if wi >= 28 { 2 } else { 1 };
    let pill_w: usize = if per_row == 1 {
        wi.min(24)
    } else {
        (wi.saturating_sub(per_row - 1)) / per_row
    };
    let h2 = inner.height >= 4 && wi >= 24; // 2 chip/pill rows fit
    let row_h: u16 = if inner.height >= 7 && wi >= 24 { 3 } else { 2 };
    let show_status = inner.height >= 8 && wi >= 24; // full: status + 2 pill rows
    let mut cons: Vec<Constraint> = vec![];
    if show_status {
        cons.push(Constraint::Length(1));
    }
    let nrows: usize = if h2 { 2 } else { 1 };
    for _ in 0..nrows {
        cons.push(Constraint::Length(row_h));
    }
    cons.push(Constraint::Min(0));
    let q_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(cons)
        .split(inner);
    let grid_y0 = if show_status { q_rows[0].y + 1 } else { q_rows[0].y };
    if show_status {
        let status_line = if !connected {
            Span::styled(" disconnected — r repairs", Style::default().fg(RED))
        } else if app.status.pilot {
            Span::styled(format!(" toggle → {} · auto guard on", next_mode), Style::default().fg(FAINT))
        } else {
            Span::styled(format!(" toggle → {} · auto off", next_mode), Style::default().fg(FAINT))
        };
        f.render_widget(
            Line::from(vec![status_line]),
            Rect { x: inner.x, y: q_rows[0].y, width: inner.width, height: 1 },
        );
    }
    for (gi, a) in acts.iter().enumerate() {
        if !h2 && gi >= 3 { break; } // tiny tiles: first row only
        let row = gi / per_row;
        let col = gi % per_row;
        let area = Rect {
            x: inner.x + (col * (pill_w + 1)) as u16,
            y: grid_y0 + (row * row_h as usize) as u16,
            width: (pill_w as u16).min(inner.width),
            height: row_h,
        };
        if area.width < 4 || area.y + area.height > inner.y + inner.height {
            continue;
        }
        let hover = app.hover.as_deref() == Some(a.id);
        let flash = flash_id == Some(a.id);
        app.btn_rects.push((area, a.id));
        if row_h >= 3 {
            quick_pill(f, area, a, hover, flash);
        } else {
            quick_chip1(f, area, a, hover, flash);
        }
    }
    if app.status.connected && app.status.mode != "off" {
        format!("{} · {}", app.status.mode, profile_show(&app.status.profile))
    } else if app.status.connected {
        "standby".into()
    } else {
        "disconnected".into()
    }
}

/// 1-row chip (tiny tile fallback): [M] 󰝚 music
fn quick_chip1(f: &mut Frame, area: Rect, a: &QAct, hover: bool, flash: bool) {
    let (txc, bgc, kbg) = if a.armed {
        (BG, FG, BG)
    } else if flash {
        (FLASH_COL, TILE2, FLASH_COL)
    } else if hover {
        (FG, TILE2, FG)
    } else {
        (DIM, TILE, TILE2)
    };
    let spans = vec![
        Span::styled(
            format!(" {}", a.key),
            Style::default().fg(kbg).bg(bgc).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ", Style::default().bg(bgc)),
        Span::styled(a.icon, Style::default()
            .fg(if a.armed { BG } else { txc }).bg(bgc).add_modifier(Modifier::BOLD)),
        Span::styled(" ", Style::default().bg(bgc)),
        Span::styled(
            a.label,
            Style::default().fg(if a.armed { BG } else { txc }).bg(bgc)
                .add_modifier(if a.armed { Modifier::BOLD } else { Modifier::empty() }),
        ),
    ];
    f.render_widget(Line::from(spans), area);
}

/// pango-verified: night (ANC on) · waves (ambient) · headset-slash (off)
const POD_ICONS: [&str; 3] = ["󰖔", "󰞍", "󰋐"];

/// segmented bar with left→right gradient over the filled part; `hot_from`
/// marks segments flashing in ACCENT (recent change), if any
fn gradient_bar(glyph: &str, n: usize, filled: usize, hot_from: Option<usize>) -> Vec<Span<'static>> {
    let filled = filled.min(n);
    let mut spans = Vec::with_capacity(4);
    let hot = hot_from.unwrap_or(filled).min(filled);
    if hot > 0 {
        spans.push(Span::styled(glyph.repeat(hot), Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)));
    }
    if filled > hot {
        // gradient: mid portion darker, last 6 cells bright head
        let rest = filled - hot;
        let head = rest.min(6);
        let mid = rest - head;
        if mid > 0 {
            spans.push(Span::styled(glyph.repeat(mid), Style::default().fg(VOL_MID)));
        }
        if head > 0 {
            spans.push(Span::styled(glyph.repeat(head), Style::default().fg(VOL_HI).add_modifier(Modifier::BOLD)));
        }
    }
    if n > filled {
        spans.push(Span::styled(glyph.repeat(n - filled), Style::default().fg(FAINT)));
    }
    spans
}

fn pod_idx(label: &str) -> usize {
    match label {
        "ambient" => 1,
        "off" => 2,
        _ => 0,
    }
}

fn batt_icon(b: u8) -> &'static str {
    match b {
        x if x > 95 => "󰁹",
        x if x > 85 => "󰂂",
        x if x > 75 => "󰂁",
        x if x > 65 => "󰂀",
        x if x > 55 => "󰁿",
        x if x > 45 => "󰁾",
        x if x > 35 => "󰁽",
        x if x > 25 => "󰁼",
        x if x > 15 => "󰁻",
        x if x > 5 => "󰁺",
        _ => "󰂃",
    }
}

/// tiles build up staggered at start: borders pulse ACCENT briefly
fn stagger_border(app: &App, idx: usize) -> Option<Color> {
    let start = app.start_at.elapsed().as_millis() as u16;
    let slot = 60u16.saturating_mul(idx as u16);
    if start < slot + 220 {
        Some(ACCENT)
    } else {
        None
    }
}

/// distribute `total` rows among tiles: start at mins, grow round-robin in
/// `order` until caps reached. Keeps layouts alive at any terminal size.
fn grow(total: u16, mins: &[u16], caps: &[u16], order: &[usize]) -> Vec<u16> {
    let mut hs: Vec<u16> = mins.to_vec();
    let mut rem = total.saturating_sub(hs.iter().copied().sum());
    let mut idx = 0;
    while rem > 0 {
        let &o = order.get(idx % order.len()).unwrap();
        idx += 1;
        if hs[o] < caps[o] {
            hs[o] += 1;
            rem -= 1;
        } else if order.iter().all(|&k| hs[k] >= caps[k]) {
            break;
        }
    }
    // leftovers beyond all caps go round-robin to the LAST tiles in order
    // (most elastic widgets), so tall terminals fill instead of blanking
    if rem > 0 {
        let tail: Vec<usize> = order.iter().rev().copied().collect();
        let mut ti = 0;
        while rem > 0 && !tail.is_empty() {
            let o = tail[ti % tail.len()];
            ti += 1;
            hs[o] += 1;
            rem -= 1;
        }
    }
    hs
}

fn lerp_col(a: Color, b: Color, t: f32) -> Color {
    let (ar, ag, ab) = match a {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => (0, 0, 0),
    };
    let (br, bg_, bb) = match b {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => (0, 0, 0),
    };
    let t = t.clamp(0.0, 1.0);
    Color::Rgb(
        (ar as f32 + (br as f32 - ar as f32) * t) as u8,
        (ag as f32 + (bg_ as f32 - ag as f32) * t) as u8,
        (ab as f32 + (bb as f32 - ab as f32) * t) as u8,
    )
}

/// prettify pulseaudio profile name: a2dp-sink-sbc_xq → a2dp sbc_xq
fn profile_show(p: &str) -> String {
    let p = p.trim();
    if p.is_empty() || p == "off" {
        return "off".into();
    }
    if p.starts_with("a2dp-sink") {
        // "a2dp-sink-aac" -> "aac", "a2dp-sink" -> "a2dp"
        let tail = p.strip_prefix("a2dp-sink").unwrap_or(p).trim_start_matches('-');
        return if tail.is_empty() { "a2dp".into() } else { tail.replace('_', " ") };
    }
    if p.starts_with("headset-head-unit") {
        // "headset-head-unit-msbc" -> "hfp msbc"/"hfp"
        let tail = p.strip_prefix("headset-head-unit").unwrap_or(p).trim_start_matches('-');
        return if tail.is_empty() { "hfp".into() } else { format!("hfp {}", tail.replace('_', " ")) };
    }
    p.replace('_', " ")
}

fn level_char(v: f32) -> &'static str {
    const L: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
    L[(v * 7.9).clamp(0.0, 7.0) as usize]
}

/// mirror oscilloscope: waveform swings around a center axis, head sample
/// glows in ACCENT, older frames fade FAINT→FG; falls back to the 1-row
/// mini wave when only one line is available.

// ---------------------------------------------------------------- media tile

fn fmt_m(s: i64) -> String {
    format!("{}:{:02}", (s / 60_000_000).abs(), (s / 1_000_000).abs() % 60)
}

fn media_transport_idents() -> [(&'static str, &'static str); 6] {
    // (id, icon) — ids referenced by btn_rects click routing
    [("m_prev", "󰒮"), ("m_play", "󰐊"), ("m_next", "󰒭"),
     ("m_stop", "󰓱"), ("m_back", "󰅁"), ("m_fwd", "󰅂")]
}

fn media_tile(f: &mut Frame, area: Rect, app: &mut App) {
    let player = app.media.player.clone();
    let w = area.width;
    let h = area.height;
    // one-row compact (stacked, media_h=3 → inner h=1): "󰝚 spotify · playing · track"
    if h < 2 {
        let p = match app.media.player.clone() {
            Some(p) => p,
            None => {
                f.render_widget(
                    Line::from(Span::styled(" 󰝚 media player — player with MPRIS (spotify, mpv, firefox …)", Style::default().fg(FAINT))),
                    Rect { x: area.x + 1, y: area.y, width: w.saturating_sub(2), height: 1 },
                );
                return;
            }
        };
        let stat = match p.status.as_str() {
            "playing" => "playing",
            "paused" => "paused",
            _ => "stop",
        };
        let track = if !p.title.is_empty() {
            if p.artist.is_empty() { p.title.clone() } else { format!("{} — {}", p.artist, p.title) }
        } else {
            String::new()
        };
        let mut spans = vec![
            Span::styled(" 󰝚 ", Style::default().fg(if p.status == "playing" { GREEN } else { DIM })),
            Span::styled(p.identity, Style::default().fg(FG).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" · {}", stat), Style::default().fg(DIM)),
        ];
        if !track.is_empty() {
            spans.push(Span::styled(" · ", Style::default().fg(FAINT)));
            spans.push(Span::styled(track, Style::default().fg(DIM)));
        }
        if !app.media.calls.is_empty() {
            spans.push(Span::styled(
                format!("   󰔰{}", app.media.calls.join("·")),
                Style::default().fg(ACCENT),
            ));
        }
        f.render_widget(
            Line::from(spans),
            Rect { x: area.x + 1, y: area.y, width: w.saturating_sub(2), height: 1 },
        );
        return;
    }
    if w < 16 {
        return;
    }

    // row plan (area = tile INNER): optional call banner, info, track,
    // progress, buttons on last row
    let has_calls = !app.media.calls.is_empty();
    let btn_y = area.y + h.saturating_sub(1);
    let prog_y = btn_y.saturating_sub(1);
    // vertical centering: when the tile has spare rows (huge terminals),
    // pin progress+buttons to the bottom and center banner/info/track
    let room = prog_y.saturating_sub(area.y + if has_calls { 3 } else { 2 });
    let pad = room / 2;
    let banner_y = area.y;
    let info_y = area.y + if has_calls { 1 } else { 0 } + pad;
    let track_y = info_y + 1;
    // call banner: any client capturing (Teams, meet, obs…) — colored chip
    if has_calls {
        let names = app.media.calls.join(", ");
        let chip = format!(" 󰔰 {} {}", names, if app.media.calls.len() == 1 { "in call (mic)" } else { "in call" });
        let cw = ((names.chars().count() + 22) as u16).min(w.saturating_sub(2));
        f.render_widget(
            Line::from(Span::styled(
                chip,
                Style::default().fg(BG).bg(if app.status.mode == "mic" { ACCENT } else { GREEN })
                    .add_modifier(Modifier::BOLD),
            )),
            Rect { x: area.x + 1, y: banner_y, width: cw, height: 1 },
        );
    }


    if player.is_none() {
        // vertically center the hint when the tile is taller than 1 row
        let msg_y = area.y + (h.saturating_sub(1)) / 2;
        f.render_widget(
            Line::from(vec![
                Span::styled(" 󰝚 ", Style::default().fg(DIM)),
                Span::styled("no media playback — player with MPRIS (spotify, firefox, mpv …)", Style::default().fg(FAINT)),
            ]),
            Rect { x: area.x, y: msg_y, width: w.saturating_sub(1), height: 1 },
        );
        return;
    }
    let p = player.unwrap();
    let playing = p.status == "playing";

    // line 1: icon · identity · status
    let ident = if p.identity.is_empty() {
        "player".to_string()
    } else {
        p.identity.clone()
    };
    let stat = match p.status.as_str() {
        "playing" => "playing",
        "paused" => "paused",
        _ => "stopped",
    };
    let l1 = Line::from(vec![
        Span::styled(" 󰝚 ", Style::default().fg(if playing { GREEN } else { DIM })),
        Span::styled(ident, Style::default().fg(FG).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" · {}", stat), Style::default().fg(DIM)),
        if p.length_us > 0 {
            Span::styled(format!("  {}", fmt_m(p.length_us)), Style::default().fg(FAINT))
        } else {
            Span::raw("")
        },
    ]);
    f.render_widget(l1, Rect { x: area.x, y: info_y, width: w.saturating_sub(1), height: 1 });

    // track info line: artist — title · album
    let track = if !p.title.is_empty() {
        let mut t = if p.artist.is_empty() {
            p.title.clone()
        } else {
            format!("{} — {}", p.artist, p.title)
        };
        if !p.album.is_empty() {
            t.push_str(&format!(" · {}", p.album));
        }
        t
    } else {
        String::new()
    };
    if !track.is_empty() && track_y < btn_y {
        let tw = w.saturating_sub(3) as usize;
        let shown: String = if track.chars().count() > tw {
            format!("{}…", &track.chars().take(tw.saturating_sub(1)).collect::<String>())
        } else {
            track.clone()
        };
        f.render_widget(
            Line::from(Span::styled(shown, Style::default().fg(DIM))),
            Rect { x: area.x + 1, y: track_y, width: w.saturating_sub(1), height: 1 },
        );
    }
    let age = media_cache_mtime()
        .and_then(|t| t.elapsed().ok())
        .unwrap_or_default();
    if p.length_us > 0 && h >= 4 && prog_y > track_y {
        let pos = live_position(&p, age);
        let frac = (pos as f64 / p.length_us as f64).clamp(0.0, 1.0);
        let prow = Rect {
            x: area.x + 1,
            y: prog_y,
            width: w.saturating_sub(13),
            height: 1,
        };
        spans_track_progress(f, prow, frac, pos, p.length_us, playing);
    }

    // transport buttons (hover/flash driven by app.btn_rects)
    if h >= 4 {
        let btns = media_transport_idents();
        let icon_y = btn_y;
        let bx = area.x + 1;
        let play_glyph = if playing { "󰏤" } else { "󰐊" };
        for (i, (id, icon)) in btns.iter().enumerate() {
            let icon = if *id == "m_play" { play_glyph } else { *icon };
            let hover = app.hover.as_deref() == Some(*id);
            let flash = app.btn_flash.map(|(t, at)| t == *id && at.elapsed() < Duration::from_millis(300)).unwrap_or(false);
            let col = if flash {
                FLASH_COL
            } else if hover {
                FG
            } else {
                DIM
            };
            let x = bx + (i as u16 * 5);
            let rect = Rect { x: x.saturating_sub(1), y: icon_y, width: 5, height: 1 };
            f.render_widget(
                Line::from(Span::styled(icon.to_string(), Style::default().fg(col).add_modifier(Modifier::BOLD))),
                Rect { x, y: icon_y, width: 3, height: 1 },
            );
            app.btn_rects.push((rect, *id));
        }
    }
}

fn spans_track_progress(f: &mut Frame, row: Rect, frac: f64, pos: i64, len: i64, playing: bool) {
    let w = row.width as usize;
    if w < 6 {
        return;
    }
    let bar_w = w - 10; // " m:ss[ /m:ss]"
    let filled_n = (bar_w as f64 * frac) as usize;
    let mut spans: Vec<Span<'static>> = vec![
        Span::styled(format!("{} ", fmt_m(pos)), Style::default().fg(if playing { FG } else { FAINT })),
    ];
    for i in 0..bar_w {
        let col = if i < filled_n { if playing { ACCENT } else { DIM } } else { FAINT };
        spans.push(Span::styled(if i < filled_n { "━" } else { "╺" }, Style::default().fg(col)));
    }
    spans.push(Span::styled(
        format!(" {}", fmt_m(len)),
        Style::default().fg(FAINT),
    ));
    f.render_widget(Line::from(spans), row);
}

fn draw_scope(f: &mut Frame, inner: Rect, area: Rect, app: &App) {
    let width = (area.width as usize).min(220);
    if width < 2 {
        return;
    }
    let m = app.mic.samples.len();
    let start = m.saturating_sub(width);
    let slice: &[f32] = &app.mic.samples[start..];
    let col_at = |t: f32, head: bool| -> Color {
        if head {
            ACCENT
        } else {
            lerp_col(FAINT, FG, t * 0.9)
        }
    };
    let lvl = |v: f32| -> &'static str {
        const L: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
        L[(v * 7.9).clamp(0.0, 7.0) as usize]
    };
    if area.height == 1 {
        let pad = width.saturating_sub(slice.len());
        let mut spans: Vec<Span<'static>> = vec![Span::raw(" ".repeat(pad))];
        for (i, v) in slice.iter().enumerate() {
            let is_head = i == slice.len() - 1;
            let t = i as f32 / width.max(1) as f32;
            spans.push(Span::styled(
                lvl(*v).to_string(),
                Style::default().fg(col_at(t, is_head)),
            ));
        }
        f.render_widget(
            Line::from(spans),
            Rect { x: inner.x + 1, y: area.y, width: area.width, height: 1 },
        );
        return;
    }
    let rows = area.height as usize;
    let half = (rows / 2).max(1);
    let usable = width.min(220);
    let s2: Vec<f32> = slice.iter().take(usable).copied().collect();
    let pad = 0usize;
    let db_scale = true; // right-side dB ruler
    let center = half - 1; // axis row index
    // dead signal (all-zero samples, e.g. suspended mic): show the idle scope
    let dead = s2.is_empty() || s2.iter().all(|v| *v < 0.01);
    // idle scope: centered note on the axis, no fake wave
    if dead {
        let mid = Rect { x: inner.x + 1, y: area.y + center as u16, width: area.width, height: 1 };
        f.render_widget(
            Line::from(Span::styled(
                " muted — mic picks up nothing".to_string(),
                Style::default().fg(FAINT),
            )),
            mid,
        );
        return;
    }
    // grid dots on the axis (every 6th col), wave cells elsewhere
    for ri in 0..rows {
        let y = area.y + ri as u16;
        let dy = (center as i32 - ri as i32).unsigned_abs() as usize;
        let mut spans: Vec<Span<'static>> = vec![Span::raw(" ".repeat(pad))];
        for (i, v) in s2.iter().enumerate() {
            let is_head = i == s2.len() - 1;
            let t = i as f32 / usable.max(1) as f32;
            let col = col_at(t, is_head);
            // amplitude in half-row units: |v| → 0..half+1 rows of excursion
            let exc = (*v as f32 * (half as f32 + 0.5)).round() as i32;
            let on = if exc == 0 {
                dy == 0
            } else {
                // mirrored bar: rows covering the excursion both sides
                (dy as i32) < exc
            };
            let ch = if db_scale && i % 12 == 0 && !on {
                "·"
            } else {
                lvl((exc as f32 / (half as f32 + 0.5)).clamp(0.0, 1.0).abs())
            };
            spans.push(Span::styled(
                if on { ch.to_string() } else if dy == 0 { "·".to_string() } else { " ".to_string() },
                Style::default().fg(col),
            ));
        }
        if db_scale {
            // right-side dB scale: +, levels, 0 marker
            let label = if ri == 0 {
                "+ "
            } else if ri == rows - 1 {
                "- "
            } else if ri == center {
                "0 "
            } else {
                "  "
            };
            spans.push(Span::styled(label, Style::default().fg(FAINT)));
        }
        f.render_widget(
            Line::from(spans),
            Rect { x: inner.x + 1, y, width: area.width, height: 1 },
        );
    }
}

// ---------------------------------------------------------------- draw

fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    f.render_widget(Paragraph::new("").style(Style::default().bg(BG)), area);
    let w = area.width;
    let h = area.height;
    app.frame = app.frame.wrapping_add(1);
    app.btn_rects.clear();

    let fits_wide = w >= 78 && h >= 13;
    let fits_narrow = w >= 50 && h >= 17;
    if !fits_wide && !fits_narrow {
        f.render_widget(
            Paragraph::new(Span::styled(
                if h < 13 && w < 78 && w >= 50 { "make it wider (min 78 columns)" } else { "terminal too small (min 50×17 · 78×13)" },
                Style::default().fg(DIM),
            ))
            .alignment(ratatui::layout::Alignment::Center),
            area,
        );
        return;
    }
    let wide = fits_wide;
    let header_h = 1u16;
    let footer_h = 1u16;
    let rows = h - header_h - footer_h;
    let with_media = wide && rows >= 16;
    let (main_h, vq_h, mic_h, media_h, dev_h, anc_h, vol_h, quick_h, stacked) = if w >= 140 {
        // fullscreen wide: main pair on top, then media LEFT and mic RIGHT
        // in one shared row
        let two_col = rows >= 14;
        let media_min = if with_media { 6 } else { 0 };
        let hs = if two_col {
            // shared media/mic row: row height = max(media, mic). Caps are
            // the "comfort" minimums; grow()'s tail pass keeps feeding BOTH
            // (they end the order, so equal shares go to media+mic) —
            // tall terminals stretch the media/mic row instead of blanking
            grow(
                rows,
                &[5, 4, media_min, 3],
                &[7, 6, if with_media { 9 } else { 0 }, 9],
                &[0, 1, 3, 2],
            )
        } else {
            grow(rows, &[5, 4, 0, 0], &[7, 6, 0, 0], &[0, 1])
        };
        (hs[0], hs[1], hs[3], hs[2], 0, 0, 0, 0, false)
    } else {
        let with_mic = rows >= 19;
        // indices: 0=dev 1=anc 2=vol 3=quick 4=mic 5=media — grow volume, then
        // anc, mic, device details, quick chips last; media gets the scraps
        let hs = if with_mic {
            grow(rows, &[3, 5, 4, 4, 3, 3], &[4, 7, 6, 6, 8, 6], &[2, 1, 4, 0, 3])
        } else {
            grow(rows, &[3, 5, 4, 4, 0, 0], &[4, 7, 6, 6, 0, 0], &[2, 1, 0, 3])
        };
        (0, 0, hs[4], hs[5], hs[0], hs[1], hs[2], hs[3], true)
    };
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints({
            let mut c = vec![Constraint::Length(header_h)];
            if stacked {
                c.push(Constraint::Length(dev_h));
                c.push(Constraint::Length(anc_h));
                c.push(Constraint::Length(media_h));
                c.push(Constraint::Length(vol_h));
                c.push(Constraint::Length(quick_h));
            } else {
                c.push(Constraint::Length(main_h));
                c.push(Constraint::Length(vq_h));
                if media_h > 0 || mic_h > 0 {
                    // media LEFT and mic RIGHT share one ELASTIC row: the
                    // Min constraint absorbs leftover rows on tall terminals
                    // (no dead space below the tiles)
                    c.push(Constraint::Min(media_h.max(mic_h)));
                }
            }
            c.push(Constraint::Length(footer_h));
            c
        })
        .split(area);

    // shared content margins
    let mx = (w / 12).max(3).min(12);
    let content_w = w.saturating_sub(mx * 2);

    // header: breathing dot (green↔light-green), name, battery right
    let name = app.status.name.clone().unwrap_or_else(|| "WH-1000XM5".into());
    let pulse = (app.start_at.elapsed().as_millis() % 2400) < 1200;
    let dot_col = if app.status.connected {
        if pulse { Color::Rgb(142, 240, 168) } else { GREEN }
    } else {
        RED
    };
    let mut head = vec![
        Span::styled(" ● ", Style::default().bg(dot_col).fg(dot_col)),
        Span::raw(" "),
        Span::styled(name.clone(), Style::default().fg(FG).add_modifier(Modifier::BOLD)),
        Span::styled("  󰂯", Style::default()
            .fg(if app.status.connected { ACCENT } else { FAINT })),
    ];
    // mode chip in header (instant orientation) — colored per mode
    if app.status.connected {
        let (mcol, mtxt) = match app.status.mode.as_str() {
            "music" => (GREEN, " music"),
            "mic" => (ACCENT, " mic"),
            _ => (DIM, " standby"),
        };
        head.push(Span::styled(
            mtxt,
            Style::default().bg(mcol).fg(if mcol == DIM { FG } else { BG })
                .add_modifier(Modifier::BOLD),
        ));
    }
    if app.status.pilot {
        head.push(Span::styled("  󰁨 auto", Style::default().fg(DIM)));
    }
    if !app.media.calls.is_empty() {
        head.push(Span::styled("  ", Style::default()));
        head.push(Span::styled(
            format!(" 󰔰 {}", app.media.calls.join("+")),
            Style::default().bg(ACCENT).fg(BG).add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(b) = app.status.battery {
        head.push(Span::raw("    "));
        head.push(Span::styled(
            batt_icon(b),
            Style::default().fg(if b < 20 { RED } else if b < 40 { ORANGE } else { FG }),
        ));
        head.push(Span::styled(format!(" {}%", b), Style::default().fg(FG).add_modifier(Modifier::BOLD)));
    }
    f.render_widget(Paragraph::new(Line::from(head)), outer[0]);

    // content rows: either two-col pairs or stacked singles
    let (dev_area, anc_area, vol_area, quick_area, media_area): (Rect, Rect, Rect, Rect, Rect);
    if stacked {
        dev_area = outer[1];
        anc_area = outer[2];
        media_area = outer[3];
        vol_area = outer[4];
        quick_area = outer[5];
    } else {
        media_area = if media_h > 0 {
            outer[3]
        } else {
            Rect::default()
        };
        let pair = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(46), Constraint::Percentage(54)].to_vec());
        let r1 = pair.clone().split(Rect {
            x: mx,
            y: outer[1].y,
            width: content_w,
            height: main_h,
        });
        dev_area = r1[0];
        anc_area = r1[1];
        let r2 = pair.split(Rect {
            x: mx,
            y: outer[2].y,
            width: content_w,
            height: vq_h,
        });
        vol_area = r2[0];
        quick_area = r2[1];
    }

    // ---------- device tile
    let dev_inner = tile(f, dev_area, Some(("device", DIM)), stagger_border(app, 0));
    {
        let dw = dev_inner.width.saturating_sub(2);
        let h_in = dev_inner.height as usize;
        // 3 content rows when room; degrade: drop mode row, then single line
        let d_rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints(
                if h_in >= 3 {
                    vec![Constraint::Length(1), Constraint::Length(1), Constraint::Length(1), Constraint::Min(0)]
                } else {
                    vec![Constraint::Min(0)]
                },
            )
            .split(dev_inner);
        let (mode_txt, mode_dim) = match app.status.mode.as_str() {
            "music" => ("music", false),
            "mic" => ("mic", false),
            _ => ("standby", true),
        };
        if h_in == 1 {
            // compressed single line: name + mode
            f.render_widget(
                Line::from(vec![
                    Span::styled(" 󰋎 ", Style::default().fg(if app.status.connected { FG } else { DIM })),
                    Span::styled("WH-1000XM5", Style::default().fg(FG).add_modifier(Modifier::BOLD)),
                    Span::styled(
                        if app.status.connected {
                            format!("  ·  {}", profile_show(&app.status.profile))
                        } else {
                            "  ·  disconnected · press r".to_string()
                        },
                        Style::default().fg(if app.status.connected { DIM } else { RED }),
                    ),
                ]),
                Rect { x: dev_inner.x + 1, y: d_rows[0].y, width: dw, height: 1 },
            );
        } else {
            let y = [d_rows[0].y, d_rows[0].y + 1, d_rows[0].y + 2];
            f.render_widget(
                Line::from(vec![
                    Span::styled("󰋎", Style::default().fg(if app.status.connected { FG } else { DIM })),
                    Span::raw("  "),
                    Span::styled("SONY", Style::default().fg(DIM).add_modifier(Modifier::BOLD)),
                    Span::raw(" "),
                    Span::styled("WH-1000XM5", Style::default().fg(FG).add_modifier(Modifier::BOLD)),
                ]),
                Rect { x: dev_inner.x + 1, y: y[0], width: dw, height: 1 },
            );
            if h_in >= 3 {
                let codec = app.status.codec.clone().unwrap_or_default();
                let mpill_col = match app.status.mode.as_str() {
                    "music" => GREEN,
                    "mic" => ACCENT,
                    _ => TILE2,
                };
                f.render_widget(
                    Line::from(vec![
                        Span::styled("mode ", Style::default().fg(DIM)),
                        Span::styled(
                            mode_txt,
                            Style::default().bg(mpill_col)
                                .fg(if mode_dim { DIM } else { BG })
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(" "),
                        Span::styled(codec.to_lowercase(), Style::default().bg(TILE2).fg(DIM)),
                    ]),
                    Rect { x: dev_inner.x + 1, y: y[1], width: dw, height: 1 },
                );
                f.render_widget(
                    Line::from(vec![
                        Span::styled("󰂯 ", Style::default().fg(if app.status.connected { GREEN } else { FAINT })),
                        Span::styled(
                            if app.status.connected {
                                format!("connected · {}", profile_show(&app.status.profile))
                            } else {
                                "disconnected · press r".to_string()
                            },
                            Style::default().fg(if app.status.connected { DIM } else { RED }),
                        ),
                    ]),
                    Rect { x: dev_inner.x + 1, y: y[2], width: dw, height: 1 },
                );
            }
        }
    }
    // ---------- noise cancel tile
    let anc_inner = tile(f, anc_area, Some(("noise cancel", DIM)), stagger_border(app, 1));
    {
        let anc = app.anc();
        let is_on = [anc == "on", anc == "ambient", anc == "off"];
        let full = ["noise cancel", "ambient", "off"];
        let short = ["nc", "amb", "off"];
        let anc_inner_h = anc_inner.height;
        let ring_mode = anc_inner_h >= 4;
        let bar_room = anc_inner_h >= 5;
        let ring_h = if ring_mode { 4u16 } else { 1 };
        let ring_rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints({
                let mut c = vec![Constraint::Length(ring_h), Constraint::Min(0)];
                if bar_room {
                    c.push(Constraint::Length(1));
                }
                c
            })
            .split(anc_inner);
        let cells = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(33), Constraint::Percentage(34), Constraint::Percentage(33)].to_vec())
            .split(Rect { x: anc_inner.x, y: ring_rows[0].y, width: anc_inner.width, height: ring_h });
        for i in 0..3 {
            let hover = app.hover.as_deref() == Some(full[i]);
            let flash = app
                .pod_flash
                .map(|(fi, at)| fi == i && at.elapsed() < Duration::from_millis(350))
                .unwrap_or(false);
            app.pod_rects[i] = cells[i];
            if ring_mode {
                let cell_w = cells[i].width;
                let label = if cell_w < 14 { short[i] } else { full[i] };
                pod(f, cells[i], label, is_on[i], hover, flash);
            } else {
                let cell_w = cells[i].width;
                let label = if cell_w >= 16 { full[i] } else { short[i] };
                pod_inline(f, cells[i], label, POD_ICONS[i], is_on[i], hover, flash);
            }
        }
        // anc level bar (bottom row)
        if bar_room {
            if let Some(lvl) = app.status.anc_level {
                if anc == "on" || anc == "ambient" {
                    let nseg = (anc_inner.width.saturating_sub(12)) as usize;
                    if nseg >= 4 {
                        let filled = (((lvl as f64 / 28.0) * nseg as f64).round() as usize).clamp(1, nseg);
                        f.render_widget(
                            Line::from({
                                let mut v = vec![Span::styled(format!(" {:>2} ", lvl), Style::default().fg(DIM))];
                                v.extend(gradient_bar("━", nseg, filled, None));
                                v.push(Span::styled(
                                    format!(" {:.0}% ", lvl as f64 * 100.0 / 28.0),
                                    Style::default().fg(DIM),
                                ));
                                v
                            }),
                            Rect { x: anc_inner.x, y: ring_rows[2].y, width: anc_inner.width, height: 1 },
                        );
                    }
                }
            }
        }
    }

    // ---------- volume tile
    let vol_inner = tile(f, vol_area, Some(("volume", DIM)), stagger_border(app, 2));
    {
        let vol = app.status.volume.unwrap_or(0);
        let vol_show = ((vol as i16) + app.vol_bump as i16).clamp(0, 100) as u8;
        let muted = app.status.muted.unwrap_or(false);
        let vi = if muted { "󰝟" } else if vol_show > 60 { "󰕾" } else if vol_show > 25 { "󰖀" } else { "󰕿" };
        let pad_top = vol_inner.height.saturating_sub(2) / 2;
        let v_rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints({
                let c = vec![
                    Constraint::Length(pad_top),
                    Constraint::Length(1),
                    Constraint::Length(1),
                    Constraint::Min(0),
                ];
                c
            })
            .split(vol_inner);
        // top row: icon (click = mute toggle) · % (hover ghost % when dragging)
        let ghost = app.vol_hover_x.filter(|_| app.hover.as_deref() == Some("volume"));
        let pct_text = if let Some(gx) = ghost {
            let g = (((gx - vol_inner.x).max(0) as f64 / vol_inner.width.max(1) as f64) * 100.0)
                .round()
                .clamp(0.0, 100.0) as u8;
            format!("{:>3}% ", g)
        } else {
            format!("{:>3}%{}", vol_show, if muted { " · muted" } else { "" })
        };
        f.render_widget(
            Line::from(vec![
                Span::styled(format!(" {}", vi), Style::default().fg(if muted { DIM } else { FG })),
                Span::raw("  "),
                Span::styled(
                    pct_text,
                    Style::default()
                        .fg(if ghost.is_some() { ACCENT } else { FG })
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            v_rows[1],
        );
        let n = vol_inner.width.saturating_sub(6) as usize;
        if n >= 4 {
            let filled = ((vol_show as f64 / 100.0) * n as f64).round() as usize;
            let prev = app.status.volume.unwrap_or(vol_show);
            let hot_from = if app.vol_bump != 0 {
                Some((prev.min(vol_show) as usize).min(n))
            } else {
                None
            };
            f.render_widget(
                Line::from({
                    let mut v = vec![Span::raw("  ".to_string())];
                    v.extend(gradient_bar("┃", n, filled, hot_from));
                    Line::from(v)
                }),
                v_rows[2],
            );
            app.vol_rect = v_rows[2];
        }
    }

    // ---------- quick actions (new pill grid)
    let quick_inner = tile(f, quick_area, Some(("quick", DIM)), stagger_border(app, 3));
    let quick_ui = quick_tile(f, quick_inner, app);
    // ---------- media (MPRIS transport + calls)
    if media_area.width > 8 {
        // wide: media shares its row with mic (media LEFT 46%)
        let media_final = if !stacked && media_h > 0 && mic_h > 0 {
            let r = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(46), Constraint::Percentage(54)])
                .split(media_area);
            r[0]
        } else {
            media_area
        };
        let media_inner = tile(f, media_final, Some(("media", DIM)), stagger_border(app, if stacked { 3 } else { 4 }));
        media_tile(f, media_inner, &mut *app);
    }

    // ---------- mic scope (full width stacked; RIGHT beside media in wide)
    if mic_h > 0 {
        let mic_area = if stacked {
            outer[6]
        } else if media_h > 0 {
            // share the media row: mic takes the right part (54%)
            let r = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(46), Constraint::Percentage(54)])
                .split(outer[3]);
            r[1]
        } else {
            Rect { x: mx, y: outer[3].y, width: content_w, height: outer[3].height }
        };
        let mic_inner = tile(f, mic_area, Some(("microphone", DIM)), stagger_border(app, if stacked { 4 } else { 5 }));
        let m_rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints(if mic_inner.height >= 2 {
                // info top; scope takes everything else (the mic tile is the
                // flexible one in the shared row)
                vec![Constraint::Length(1), Constraint::Min(1)]
            } else {
                vec![Constraint::Min(0)]
            })
            .split(mic_inner);
        // info line: icon · state · dB · dsp
        let src_short = if app.mic.src.starts_with("bluez_input") {
            "xm5".to_string()
        } else if app.mic.src.is_empty() {
            String::new()
        } else if app.mic.src.starts_with("alsa_input") {
            "internal".to_string()
        } else {
            app.mic.src.chars().take(18).collect()
        };
        let info = if app.mic.live {
            vec![
                Span::styled(" 󰋎 ", Style::default().fg(GREEN)),
                Span::styled("live", Style::default().fg(FG)),
                Span::styled(
                    format!(" · {:.0} dB · {}", app.mic.db, src_short),
                    Style::default().fg(DIM),
                ),
                Span::styled(
                    if app.status.enhance { "   󰘮 dsp on" } else { "   󰘮 dsp off" },
                    Style::default().fg(if app.status.enhance { GREEN } else { FAINT }),
                ),
            ]
        } else if !app.status.connected {
            vec![
                Span::styled(" 󰋎 ", Style::default().fg(DIM)),
                Span::styled("no device", Style::default().fg(DIM)),
                Span::styled(
                    if app.status.enhance { "   󰘮 dsp on" } else { "   󰘮 dsp off" },
                    Style::default().fg(if app.status.enhance { GREEN } else { FAINT }),
                ),
            ]
        } else {
            vec![
                Span::styled(" 󰋎 ", Style::default().fg(DIM)),
                Span::styled("idle", Style::default().fg(DIM)),
                Span::styled(" — mic mode via v", Style::default().fg(DIM)),
                Span::styled(
                    if app.status.enhance { "   󰘮 dsp on" } else { "   󰘮 dsp off" },
                    Style::default().fg(if app.status.enhance { GREEN } else { FAINT }),
                ),
            ]
        };
        // 3+ rows → info line + mirror scope; single row → info only
        if mic_inner.height >= 3 {
            f.render_widget(Line::from(info), m_rows[0]);
            draw_scope(f, mic_inner, m_rows[1], app);
        } else if mic_inner.height == 2 {
            draw_scope(f, mic_inner, m_rows[0], app);
        } else {
            // one-row fallback: " 󰋎  idle" / " 󰋎 live · 6 dB ▁▅█▂.."
            let width = (mic_inner.width.saturating_sub(4) as usize).max(0);
            if width >= 6 {
                let compact_info: Vec<Span<'static>> = vec![
                    Span::styled(" 󰋎 ", Style::default().fg(if app.mic.live { GREEN } else { DIM })),
                ];
                let n = width.saturating_sub(20).min(220);
                let m = app.mic.samples.len();
                let start = m.saturating_sub(n);
                let slice: Vec<f32> = app.mic.samples[start..].to_vec();
                let mut spans = compact_info;
                if !app.mic.live {
                    spans.push(Span::styled(
                        if app.status.connected { "idle" } else { "no device" },
                        Style::default().fg(FAINT),
                    ));
                }
                if app.mic.live && n >= 4 {
                    spans.push(Span::styled(" · ", Style::default().fg(FAINT)));
                    for (i, v) in slice.iter().enumerate() {
                        let t = i as f32 / n.max(1) as f32;
                        let col = lerp_col(FAINT, ACCENT, t);
                        spans.push(Span::styled(
                            level_char(*v).to_string(),
                            Style::default().fg(col),
                        ));
                    }
                }
                f.render_widget(Line::from(spans), Rect {
                    x: mic_inner.x + 1,
                    y: m_rows[0].y,
                    width: mic_inner.width.saturating_sub(2),
                    height: 1,
                });
            }
        }
    }

    // footer: quick chips (stateful) + mode line + hints
    {
        let foot_y = h.saturating_sub(1);
        let acts = quick_actions(app);
        let mut spans: Vec<Span<'static>> = vec![];
        for a in acts.iter().take(4) {
            let on = a.armed;
            let hv = app.hover.as_deref() == Some(a.id);
            let (kbg, kfg, tbg, tfg) = if on {
                (BG, FG, ACCENT, BG)
            } else if hv {
                (FG, BG, TILE2, FG)
            } else {
                (TILE2, DIM, TILE2, DIM)
            };
            spans.push(Span::styled(
                format!(" {} ", a.key),
                Style::default().fg(kfg).bg(kbg).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                format!("{}{} ", a.icon, a.label.to_lowercase()),
                Style::default().fg(tfg).bg(tbg),
            ));
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled("+/− vol · q done", Style::default().fg(FAINT)));
        f.render_widget(Paragraph::new(Line::from(spans)), Rect { x: mx, y: foot_y, width: content_w, height: 1 });
        f.render_widget(
            Paragraph::new(Span::styled(quick_ui.clone(), Style::default().fg(DIM)))
                .alignment(ratatui::layout::Alignment::Right),
            Rect { x: mx, y: foot_y, width: content_w, height: 1 },
        );
    }

    // help overlay: key badges + description, grouped
    if app.help {
        let hw = 54u16.min(w.saturating_sub(8));
        let hh = 17u16.min(h.saturating_sub(4));
        let hr = Rect { x: (w.saturating_sub(hw)) / 2, y: (h.saturating_sub(hh)) / 2, width: hw, height: hh };
        f.render_widget(ratatui::widgets::Clear, hr);
        let hi = tile(f, hr, Some(("keys", ACCENT)), None);
        let rows: [(&str, &str, bool); 17] = [
            ("1·2·3 / a", "anc: nc · ambient · off", true),
            ("↑/↓", "anc level", true),
            ("←/→", "switch pod", true),
            ("+ / −", "volume up / down", false),
            ("M", "music (a2dp)", false),
            ("V", "mic (hfp)", false),
            ("T", "music ⇄ mic", false),
            ("E", "mic dsp (echo-cancel)", false),
            ("A", "auto guard on/off", false),
            ("R", "repair bluetooth", false),
            ("Space", "playback play/pause", false),
            ("N / B", "next / previous track", false),
            ("J / L", "−10 s / +10 s in track", false),
            ("X", "stop playback", false),
            ("F", "refresh status", false),
            ("Mouse", "click pods · click media keys · drag volume", false),
            ("q/esc", "quit (esc closes help)", false),
        ];
        for (i, (k, d, _grp)) in rows.iter().enumerate() {
            if k.is_empty() {
                continue;
            }
            let y = hi.y + i as u16;
            f.render_widget(
                Line::from(vec![
                    Span::styled(format!("  {:>8} ", k), Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
                    Span::styled(*d, Style::default().fg(DIM)),
                ]),
                Rect { x: hi.x, y, width: hi.width, height: 1 },
            );
        }
        f.render_widget(
            Line::from(Span::styled(
                " esc closes".to_string(),
                Style::default().fg(FAINT),
            )),
            Rect { x: hi.x, y: hr.y + hh - 1, width: hi.width, height: 1 },
        );
    }

    // toast overlay (bottom center)
    if let Some(t) = &app.toast {
        let elapsed = t.at.elapsed();
        let life = if t.spinning { Duration::from_secs(15) } else { Duration::from_secs(3) };
        if elapsed < life {
            let is_err = !t.spinning && (t.msg.contains("error") || t.msg.contains("fail") || t.msg.contains("no "));
            let text = if t.spinning {
                const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
                let idx = (elapsed.as_millis() / 90) % FRAMES.len() as u128;
                format!(" {} {} ", FRAMES[idx as usize], t.msg)
            } else if is_err {
                format!(" 󰘬 {} ", t.msg)
            } else {
                format!(" ✓ {} ", t.msg)
            };
            let remain = life.saturating_sub(elapsed);
            let fade = remain < Duration::from_millis(600);
            let entering = elapsed < Duration::from_millis(150);
            let border_col = if fade {
                FAINT
            } else if is_err {
                RED
            } else if entering {
                ACCENT
            } else {
                TILE_BORDER
            };
            let txt_col = if fade || t.spinning { DIM } else if is_err { RED } else { FG };
            let tw = (text.chars().count() as u16 + 2).min(w.saturating_sub(4));
            let tr = Rect { x: (w.saturating_sub(tw)) / 2, y: h.saturating_sub(6), width: tw, height: 3 };
            f.render_widget(ratatui::widgets::Clear, tr);
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    text,
                    Style::default().fg(txt_col).add_modifier(Modifier::BOLD),
                )))
                .alignment(ratatui::layout::Alignment::Center)
                .block(Block::default()
                    .borders(ratatui::widgets::Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(border_col))
                    .style(Style::default().bg(TILE))),
                tr,
            );
        }
    }
}

impl App {
    fn anc(&self) -> String {
        self.anc_pending.clone()
            .or_else(|| self.status.anc.clone())
            .unwrap_or_else(|| "on".into())
    }
}

// ---------------------------------------------------------------- actions

fn toast(app: &mut App, msg: &str, spinning: bool) {
    app.toast = Some(Toast { msg: msg.into(), at: Instant::now(), spinning });
}

fn action_direct_silent(_app: &mut App, args: &[&str]) {
    let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let Some(tx) = ACTION_TX.lock().clone() else { return };
    thread::spawn(move || {
        let refs: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
        engine_run(&refs);
        let _ = tx.send(Msg::Done(String::new()));
    });
}

fn action_direct(app: &mut App, args: &[&str], label: &str) {
    if app.busy { return; }
    app.busy = true;
    toast(app, label, true);
    let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let label = label.to_string();
    let Some(tx) = ACTION_TX.lock().clone() else { return };
    thread::spawn(move || {
        let refs: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
        let res = engine_run(&refs);
        let res = res.trim().to_string();
        let msg = if res.is_empty() { format!("{} ✓", label) } else { format!("{} · {}", label, res) };
        let _ = tx.send(Msg::Done(msg));
    });
}

fn vol_step(app: &mut App, delta: i8) {
    let cur = app.status.volume.unwrap_or(40);
    let target = ((cur as i16) + delta as i16).clamp(0, 100) as u8;
    app.vol_bump = target as i8 - cur as i8;
    app.vol_bump_at = Some(Instant::now());
    let owned = vec!["volume".to_string(), "set".to_string(), format!("{}", target)];
    let Some(tx) = ACTION_TX.lock().clone() else { return };
    thread::spawn(move || {
        let refs: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
        let _ = engine_run(&refs);
        let _ = tx.send(Msg::Done(String::new()));
    });
}

fn vol_set(app: &mut App, target: u8) {
    app.vol_bump = target as i8 - app.status.volume.unwrap_or(0) as i8;
    app.vol_bump_at = Some(Instant::now());
    let owned = vec!["volume".to_string(), "set".to_string(), format!("{}", target)];
    let Some(tx) = ACTION_TX.lock().clone() else { return };
    thread::spawn(move || {
        let refs: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
        let _ = engine_run(&refs);
        let _ = tx.send(Msg::Done(String::new()));
    });
}

fn pod_index(anc: &str) -> usize {
    match anc { "ambient" => 1, "off" => 2, _ => 0 }
}

fn pod_click(app: &mut App, i: usize, cur: &str) {
    let target = ["on", "ambient", "off"][i];
    if cur == target {
        // already active: force a state resync without blocking the UI loop
        if let Some(tx) = ACTION_TX.lock().clone() {
            thread::spawn(move || {
                let _ = tx.send(Msg::Status(fetch_status()));
            });
        }
        return;
    }
    app.anc_pending = Some(target.to_string());
    app.anc_pending_at = Some(Instant::now());
    app.pod_flash = Some((i, Instant::now()));
    app.dirty = true;
    action_direct_silent(app, &["anc", target]);
    toast(app, POD_LABELS[i], false);
}

fn anc_level(app: &mut App, delta: i8) {
    let mode = app.anc();
    if mode == "off" { return; }
    let cur = app.status.anc_level.unwrap_or(15) as i8;
    let target = (cur + delta).clamp(1, 28) as u8;
    if target == cur as u8 { return; }
    // optimistic: bump display instantly
    app.status.anc_level = Some(target);
    action_direct_silent(app, &["anc", &mode, &target.to_string()]);
    toast(app, &format!("level {}", target), false);
}

const POD_LABELS: [&str; 3] = ["noise cancel", "ambient", "anc off"];

// ---------------------------------------------------------------- main loop

fn main() -> io::Result<()> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    let res = run_app(Terminal::new(CrosstermBackend::new(io::stdout()))?);
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture)?;
    assert!(res.is_ok(), "{:?}", res.err());
    Ok(())
}

fn recv_dirty(rx: &mpsc::Receiver<Msg>, app: &mut App) {
    while let Ok(mut msg) = rx.recv_timeout(Duration::from_millis(25)) {
        match msg {
            Msg::Status(ref mut s) => {
                if let Some(at) = app.vol_bump_at {
                    if at.elapsed() > Duration::from_millis(1500) {
                        app.vol_bump = 0;
                        app.vol_bump_at = None;
                    }
                }
                if app.vol_bump != 0 {
                    s.volume = Some(((s.volume.unwrap_or(0) as i16 + app.vol_bump as i16).clamp(0, 100)) as u8);
                }
                app.status = s.clone();
                // a running resync/spin clears when fresh state arrives
                if app.toast.as_ref().map(|t| t.spinning).unwrap_or(false)
                    && app.toast.as_ref().map(|t| t.msg == "refreshed").unwrap_or(false)
                {
                    app.toast = None;
                }
                // expire anc_pending once the engine confirms the state
                if let Some(pend) = &app.anc_pending {
                    if s.anc.as_deref() == Some(pend.as_str()) {
                        app.anc_pending = None;
                        app.anc_pending_at = None;
                    }
                }
            }
            Msg::Mic { samples, live, src, db } => {
                if !live && !app.mic.samples.is_empty() {
                    app.mic = MicView::default();
                    app.dirty = true;
                } else if live {
                    app.mic = MicView { samples, live, src, db };
                    app.dirty = true;
                }
            }
            Msg::Media(m) => app.media = m,
            Msg::Done(_) => app.busy = false,
        }
        app.dirty = true;
    }
}

fn run_app(mut terminal: Terminal<CrosstermBackend<Stdout>>) -> io::Result<()> {
    let (tx, rx) = mpsc::channel::<Msg>();
    *ACTION_TX.lock() = Some(tx.clone());

    // status refresher: pilot's cache file is the source of truth; re-send
    // only when its mtime changes (dedupe), fallback engine poll if stale
    {
        let tx = tx.clone();
        thread::spawn(move || {
            let mut last_mtime: Option<std::time::SystemTime> = None;
            let mut last_fallback = Instant::now() - Duration::from_secs(10);
            loop {
                let mtime = status_cache_mtime();
                if mtime != last_mtime {
                    last_mtime = mtime;
                    let _ = tx.send(Msg::Status(fetch_status()));
                    thread::sleep(Duration::from_millis(250));
                    continue;
                }
                // cache unchanged: throttle checks, but if it looks stale
                // (pilot dead) poll the engine less often (1 s)
                let stale = mtime
                    .map(|m| m.elapsed().map(|d| d > Duration::from_millis(2600)).unwrap_or(true))
                    .unwrap_or(true);
                if stale && last_fallback.elapsed() > Duration::from_secs(1) {
                    last_fallback = Instant::now();
                    let _ = tx.send(Msg::Status(fetch_status()));
                }
                thread::sleep(Duration::from_millis(250));
            }
        });
    }
    // mic scope sampler
    {
        let tx = tx.clone();
        thread::spawn(move || mic_thread(tx));
    }
    // media refresher: pilot rewrites media.json every ~3 s; forward on change
    {
        let tx = tx.clone();
        thread::spawn(move || {
            let mut last: Option<std::time::SystemTime> = None;
            loop {
                let m = media_cache_mtime();
                if m.is_some() && m != last {
                    last = m;
                    let _ = tx.send(Msg::Media(fetch_media()));
                }
                thread::sleep(Duration::from_millis(400));
            }
        });
    }

    let mut app = App::new();
    app.media = fetch_media();
    app.status = fetch_status();
    app.start_at = Instant::now(); // stagger starts AFTER initial fetch
    while !app.quit {
        recv_dirty(&rx, &mut app);
        let expired = app.toast.as_ref().map(|t| {
            let life = if t.spinning { Duration::from_secs(15) } else { Duration::from_secs(3) };
            t.at.elapsed() > life
        }).unwrap_or(false);
        if expired {
            app.toast = None;
            app.dirty = true;
        }
        if app.toast.is_some() || app.vol_bump_at.is_some() {
            app.dirty = true; // animated overlays redraw while visible
        }
        // keep animating until the opening stagger / pod flash is done
        if app.start_at.elapsed() < Duration::from_millis(640)
            || app.pod_flash.map(|(_, at)| at.elapsed() < Duration::from_millis(350)).unwrap_or(false)
            || (app.media.player.as_ref().map(|p| p.status == "playing").unwrap_or(false)
                && app.media.player.as_ref().map(|p| p.length_us > 0).unwrap_or(false))
        {
            app.dirty = true;
        }
        if app.dirty {
            app.frame = app.frame.wrapping_add(1);
            terminal.draw(|f| draw(f, &mut app))?;
            app.dirty = false;
        }

        while event::poll(Duration::from_millis(30))? {
            let ev = event::read()?;
            let ev_dirty = matches!(&ev, Event::Key(k) if k.kind == KeyEventKind::Press)
                || !matches!(ev, Event::Mouse(MouseEvent { kind: MouseEventKind::Moved, .. }));
            match ev {
                Event::Key(k) if k.kind == KeyEventKind::Press => match k.code {
                    KeyCode::Esc => {
                        if app.help { app.help = false; } else { app.quit = true; }
                    }
                    KeyCode::Char('q') => app.quit = true,
                    KeyCode::Char('?') | KeyCode::Char('h') => app.help = !app.help,
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => app.quit = true,
                    KeyCode::Char('1') | KeyCode::Char('a') => {
                        let cur = app.anc();
                        pod_click(&mut app, 0, &cur);
                    }
                    KeyCode::Char('2') | KeyCode::Char('s') => {
                        let cur = app.anc();
                        pod_click(&mut app, 1, &cur);
                    }
                    KeyCode::Char('3') | KeyCode::Char('d') => {
                        let cur = app.anc();
                        pod_click(&mut app, 2, &cur);
                    }
                    KeyCode::Char('+') | KeyCode::Char('=') => vol_step(&mut app, 5),
                    KeyCode::Char('-') => vol_step(&mut app, -5),
                    KeyCode::Up => anc_level(&mut app, 1),
                    KeyCode::Down => anc_level(&mut app, -1),
                    KeyCode::Left => {
                        let cur = app.anc();
                        pod_click(&mut app, (pod_index(&cur) + 2) % 3, &cur);
                    }
                    KeyCode::Right => {
                        let cur = app.anc();
                        pod_click(&mut app, (pod_index(&cur) + 1) % 3, &cur);
                    }
                    KeyCode::Char('m') => action_direct(&mut app, &["mode", "music"], "music mode"),
                    KeyCode::Char('v') => action_direct(&mut app, &["mode", "mic"], "mic mode"),
                    KeyCode::Char('t') => action_direct(&mut app, &["mode", "toggle"], "toggling"),
                    KeyCode::Char('e') => action_direct(&mut app, &["mic-enhance", "toggle"], "mic DSP"),
                    KeyCode::Char('r') => action_direct(&mut app, &["repair"], "repairing"),
                    KeyCode::Char('A') => action_direct(&mut app, &["mode", "auto"], "auto mode"),
                    KeyCode::Char(' ') => action_direct(&mut app, &["media-ctl", "0", "playpause"], "play/pause"),
                    KeyCode::Char('x') => action_direct(&mut app, &["media-ctl", "0", "stop"], "stop"),
                    KeyCode::Char('n') => action_direct(&mut app, &["media-ctl", "0", "next"], "skip"),
                    KeyCode::Char('b') => action_direct(&mut app, &["media-ctl", "0", "prev"], "backward"),
                    KeyCode::Char('j') => action_direct(&mut app, &["media-ctl", "0", "seek", "-10000000"], "−10s"),
                    KeyCode::Char('l') => action_direct(&mut app, &["media-ctl", "0", "seek", "10000000"], "+10s"),
                    KeyCode::Char('F') => {
                        if let Some(tx) = ACTION_TX.lock().clone() {
                            thread::spawn(move || {
                                let _ = tx.send(Msg::Status(fetch_status()));
                            });
                        }
                        toast(&mut app, "refreshed", true);
                    }
                    _ => {}
                },
                Event::Mouse(me) => {
                    let (x, y) = (me.column, me.row);
                    match me.kind {
                        MouseEventKind::Down(_) => {
                            if app.help {
                                app.help = false;
                                continue;
                            }
                            if app.vol_rect.width > 0 && app.vol_rect.contains(ratatui::layout::Position { x, y }) {
                                let rel = (x - app.vol_rect.x) as f64 / app.vol_rect.width as f64;
                                let target = (rel * 100.0).round().clamp(0.0, 100.0) as u8;
                                vol_set(&mut app, target);
                                continue; // consumes click
                            }
                            let mut hit = false;
                            for i in 0..3 {
                                if app.pod_rects[i].width > 0
                                    && app.pod_rects[i].contains(ratatui::layout::Position { x, y }) {
                                    let cur = app.anc();
                                    pod_click(&mut app, i, &cur);
                                    hit = true;
                                    break;
                                }
                            }
                            if !hit {
                                for (r, t) in &app.btn_rects {
                                    if r.width > 0 && r.contains(ratatui::layout::Position { x, y }) {
                                        if let Some(args) = quick_engine_args(t) {
                                            let label = match *t {
                                                "music" => "music mode",
                                                "mic" => "mic mode",
                                                "toggle" => "toggling",
                                                "dsp" => "mic DSP",
                                                "repair" => "repairing",
                                                "m_play" => "play/pause",
                                                "m_next" => "skip",
                                                "m_prev" => "backward",
                                                "m_back" => "−10s",
                                                "m_fwd" => "+10s",
                                                "auto" => "auto mode",
                                                _ => "",
                                            };
                                            app.btn_flash = Some((t, Instant::now()));
                                            if label.is_empty() {
                                                action_direct_silent(&mut app, &args);
                                            } else {
                                                action_direct(&mut app, &args, label);
                                            }
                                        }
                                        break;
                                    }
                                }
                            }
                        }
                        MouseEventKind::Drag(_) => {
                            if app.vol_rect.width > 0 && y == app.vol_rect.y
                                && x >= app.vol_rect.x && x < app.vol_rect.x + app.vol_rect.width {
                                let rel = (x - app.vol_rect.x) as f64 / app.vol_rect.width as f64;
                                let target = (rel * 100.0).round().clamp(0.0, 100.0) as u8;
                                vol_set(&mut app, target);
                            }
                        }
                        MouseEventKind::Moved => {
                            let mut hov = None;
                            let labels = ["noise cancel", "ambient", "off"];
                            for i in 0..3 {
                                if app.pod_rects[i].width > 0
                                    && app.pod_rects[i].contains(ratatui::layout::Position { x, y }) {
                                    hov = Some(labels[i].to_string());
                                }
                            }
                            if app.vol_rect.width > 0
                                && y >= app.vol_rect.y.saturating_sub(1) && y <= app.vol_rect.y + 2
                                && x >= app.vol_rect.x && x < app.vol_rect.x + app.vol_rect.width {
                                hov = Some("volume".into());
                                app.vol_hover_x = Some(x);
                            } else {
                                app.vol_hover_x = None;
                            }
                            for (r, t) in &app.btn_rects {
                                if r.contains(ratatui::layout::Position { x, y }) {
                                    hov = Some(t.to_string());
                                }
                            }
                            if hov != app.hover {
                                app.hover = hov;
                                app.dirty = true;
                            }
                        }
                        MouseEventKind::ScrollUp => vol_step(&mut app, 3),
                        MouseEventKind::ScrollDown => vol_step(&mut app, -3),
                        _ => {}
                    }
                }
                _ => {}
            }
            if ev_dirty {
                app.dirty = true;
            }
        }
    }
    SHUTDOWN.store(true, Ordering::Relaxed);
    kill_mic();
    Ok(())
}