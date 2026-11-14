//! Desktop pixel pet that reacts to your Claude Code usage.
//! Run:     pixel_pet.exe            (drag with left mouse, right-click for menu)
//! Preview: pixel_pet.exe --preview  (writes pet_preview.gif and pet_skins.gif)
//! Test:    cargo test
#![windows_subsystem = "windows"]

use serde_json::Value;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{fs, ptr};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

// Real limits come from LIMITS_FILE, written by the Claude Code status line script.
// Without it, usage is estimated from tokens (input + output + cache writes) against these:
const FIVE_H_LIMIT: f64 = 8_000_000.0;
const WEEK_LIMIT: f64 = 80_000_000.0;
const SCREAM_PCT: f64 = 80.0; // % of the 5h limit where it starts screaming
const WORKING_SECS: f64 = 90.0; // a transcript written this recently -> typing
const SLEEP_SECS: f64 = 600.0; // no activity this long -> sleeping

const W: i32 = 32; // grid cells
const H: i32 = 26;
const S: i32 = 5; // cell size in px
const KEY: u32 = 0xff00ff; // transparent color (same value as RGB and as COLORREF)
const OUT: u32 = 0x1c120e; // outline + eyes
const BODY: [&str; 12] = [
    "...XXXXXXXXXXXX...",
    "...XXXXXXXXXXXX...",
    "XXXXXXXXXXXXXXXXXX",
    "XXXXXXXXXXXXXXXXXX",
    "XXXXXXXXXXXXXXXXXX",
    "...XXXXXXXXXXXX...",
    "...XXXXXXXXXXXX...",
    "...XXXXXXXXXXXX...",
    "...XXXXXXXXXXXX...",
    "...XX.XX..XX.XX...",
    "...XX.XX..XX.XX...",
    "...XX.XX..XX.XX...",
];
const SHADE: [u32; 12] = [
    0xec8a62, 0xec8a62, 0xd97757, 0xd97757, 0xd97757, 0xd97757, 0xd97757, 0xd97757, 0xb95d3f, 0xb95d3f,
    0xb95d3f, 0xb95d3f,
]; // color per body row
const BX: i32 = 7; // body offset in the grid
const BY: i32 = 11;
const ZBIG: [&str; 5] = ["XXXXX", "...X.", "..X..", ".X...", "XXXXX"];
const ZSMALL: [&str; 4] = ["XXXX", "..X.", ".X..", "XXXX"];
const RIP: [[&str; 5]; 3] = [
    ["XX.", "X.X", "XX.", "X.X", "X.X"],
    ["XXX", ".X.", ".X.", ".X.", "XXX"],
    ["XX.", "X.X", "XX.", "X..", "X.."],
];
const STATES: [&str; 5] = ["idle", "working", "sleeping", "scream", "rip"];
const SKINS: [&str; 5] = ["none", "headphones", "crown", "cat ears", "beanie"];

type Px = HashMap<(i32, i32), u32>;

fn cells_by(rows: &[impl AsRef<str>], x0: i32, y0: i32, color: impl Fn(i32, i32) -> u32) -> Px {
    let mut out = Px::new();
    for (y, r) in rows.iter().enumerate() {
        for (x, c) in r.as_ref().chars().enumerate() {
            if c == 'X' {
                let p = (x0 + x as i32, y0 + y as i32);
                out.insert(p, color(p.0, p.1));
            }
        }
    }
    out
}

fn cells(rows: &[impl AsRef<str>], x0: i32, y0: i32, color: u32) -> Px {
    cells_by(rows, x0, y0, |_, _| color)
}

fn rect(x0: i32, y0: i32, x1: i32, y1: i32, color: u32) -> Px {
    (x0..=x1).flat_map(|x| (y0..=y1).map(move |y| ((x, y), color))).collect()
}

fn pick(five_pct: f64, week_pct: f64, idle: f64, app: &str) -> &'static str {
    if week_pct >= 100.0 { return "rip"; }
    if five_pct >= SCREAM_PCT { return "scream"; }
    if app == "closed" { return "idle"; }
    if app == "minimized" { return "sleeping"; }
    if idle < WORKING_SECS { return "working"; }
    if idle > SLEEP_SECS { return "sleeping"; }
    "idle"
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// pid -> (parent pid, lowercase exe name)
fn processes() -> HashMap<u32, (u32, String)> {
    let mut out = HashMap::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE { return out; }
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snap, &mut e);
        while ok != 0 {
            let n = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
            out.insert(e.th32ProcessID, (e.th32ParentProcessID, String::from_utf16_lossy(&e.szExeFile[..n]).to_lowercase()));
            ok = Process32NextW(snap, &mut e);
        }
        CloseHandle(snap);
    }
    out
}

/// pid -> [is_minimized, ...] for visible, titled, top-level windows.
fn windows() -> HashMap<u32, Vec<bool>> {
    unsafe extern "system" fn cb(h: HWND, lp: LPARAM) -> BOOL {
        let out = &mut *(lp as *mut HashMap<u32, Vec<bool>>);
        if IsWindowVisible(h) != 0 && GetWindowTextLengthW(h) != 0 && GetWindow(h, GW_OWNER).is_null() {
            let mut pid = 0;
            GetWindowThreadProcessId(h, &mut pid);
            out.entry(pid).or_default().push(IsIconic(h) != 0);
        }
        1
    }
    let mut out = HashMap::new();
    unsafe { EnumWindows(Some(cb), &mut out as *mut _ as LPARAM) };
    out
}

/// "open", "minimized" or "closed" for the Claude app and any terminal running the claude CLI.
fn claude_window() -> &'static str {
    // shortcut: the CLI is found as claude.exe and its window via parent processes (terminal, VS Code);
    // an npm/node-installed CLI isn't detected, match node's command line if you need it.
    let (procs, wins) = (processes(), windows());
    let mut found = vec![];
    for (&start, (_, name)) in &procs {
        if name != "claude.exe" { continue; }
        let (mut pid, mut seen) = (start, HashSet::new());
        while let Some((ppid, exe)) = procs.get(&pid) {
            if exe == "explorer.exe" || !seen.insert(pid) { break; }
            if let Some(w) = wins.get(&pid) {
                found.extend(w);
                break;
            }
            pid = *ppid;
        }
    }
    if found.is_empty() { "closed" } else if found.iter().all(|&m| m) { "minimized" } else { "open" }
}

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

fn claude_dir() -> PathBuf {
    PathBuf::from(std::env::var("USERPROFILE").unwrap_or_default()).join(".claude")
}

/// (5h %, week %) as Claude reported them to the status line, or None if not saved yet.
fn real_limits() -> Option<(f64, f64)> {
    let text = fs::read_to_string(claude_dir().join("pet_limits.json")).ok()?;
    let d: Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
    d.as_object()?;
    let t = now();
    let pct = |w: &Value| match w.get("resets_at").map_or(Some(t + 1.0), |r| r.as_f64()) {
        Some(reset) if reset > t => w.get("used_percentage").and_then(Value::as_f64).unwrap_or(0.0),
        _ => 0.0, // past reset = 0
    };
    Some((pct(&d["five_hour"]), pct(&d["seven_day"])))
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468
}

/// Unix seconds from an ISO timestamp like 2026-11-14T10:20:30.123Z (or with a +hh:mm offset).
fn parse_ts(s: &str) -> Option<f64> {
    let n = |a: usize, b: usize| s.get(a..b).and_then(|v| v.parse::<i64>().ok());
    let secs = days_from_civil(n(0, 4)?, n(5, 7)?, n(8, 10)?) * 86400 + n(11, 13)? * 3600 + n(14, 16)? * 60 + n(17, 19)?;
    let rest = s.get(19..)?;
    let frac_len = if rest.starts_with('.') { 1 + rest[1..].chars().take_while(char::is_ascii_digit).count() } else { 0 };
    let frac = if frac_len > 1 { format!("0{}", &rest[..frac_len]).parse::<f64>().ok()? } else { 0.0 };
    let off = match &rest[frac_len..] {
        "" | "Z" => 0,
        tz if tz.len() == 6 => {
            let v = tz[1..3].parse::<i64>().ok()? * 3600 + tz[4..6].parse::<i64>().ok()? * 60;
            if tz.starts_with('-') { -v } else { v }
        }
        _ => return None,
    };
    Some((secs - off) as f64 + frac)
}

type Entry = (String, f64, u64); // (message id, unix time, tokens)

#[derive(Default)]
struct Usage {
    cache: HashMap<PathBuf, ((SystemTime, u64), Vec<Entry>)>,
}

impl Usage {
    fn entries(&mut self, path: &Path, key: (SystemTime, u64)) -> &[Entry] {
        let slot = self.cache.entry(path.to_path_buf()).or_insert_with(|| ((UNIX_EPOCH, u64::MAX), vec![]));
        if slot.0 != key {
            let bytes = fs::read(path).unwrap_or_default();
            let mut out = vec![];
            for line in String::from_utf8_lossy(&bytes).lines() {
                if !line.contains("\"usage\"") { continue; }
                let Ok(d) = serde_json::from_str::<Value>(line) else { continue };
                let (m, u) = (&d["message"], &d["message"]["usage"]);
                let Some(ts) = d["timestamp"].as_str().and_then(parse_ts) else { continue };
                if !u.is_object() { continue; }
                let tok = ["input_tokens", "output_tokens", "cache_creation_input_tokens"]
                    .iter().map(|k| u[k].as_u64().unwrap_or(0)).sum();
                let id = m["id"].as_str().or(d["uuid"].as_str()).unwrap_or("");
                out.push((id.to_string(), ts, tok));
            }
            *slot = (key, out);
        }
        &slot.1
    }

    /// (5h tokens, week tokens, seconds since the last transcript write)
    fn read(&mut self) -> (f64, f64, f64) {
        // shortcut: rolling 5h/7d token windows, not Anthropic's exact reset times; tune limits above.
        let t = now();
        let mut files = vec![];
        jsonl_files(&claude_dir().join("projects"), &mut files);
        let (mut seen, mut last) = (HashMap::new(), 0.0f64);
        for p in files {
            let Ok(meta) = fs::metadata(&p) else { continue };
            let Ok(mtime) = meta.modified() else { continue };
            let mt = mtime.duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
            if t - mt > 7.0 * 86400.0 { continue; }
            last = last.max(mt);
            for (id, ts, tok) in self.entries(&p, (mtime, meta.len())) {
                seen.insert(id.clone(), (*ts, *tok)); // last line per message id wins
            }
        }
        let sum = |secs: f64| seen.values().filter(|(ts, _)| t - ts < secs).map(|&(_, tok)| tok as f64).sum::<f64>();
        (sum(5.0 * 3600.0), sum(7.0 * 86400.0), t - last)
    }
}

fn jsonl_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        match e.file_type() {
            Ok(ft) if ft.is_dir() => jsonl_files(&p, out),
            Ok(_) if p.extension().is_some_and(|x| x == "jsonl") => out.push(p),
            _ => {}
        }
    }
}

/// Paint back to front; each part gets its own dark outline.
fn layer(px: &mut Px, part: &Px, edge: bool) {
    if edge {
        for &(x, y) in part.keys() {
            for dx in -1..=1 {
                for dy in -1..=1 {
                    if !part.contains_key(&(x + dx, y + dy)) {
                        px.insert((x + dx, y + dy), OUT);
                    }
                }
            }
        }
    }
    px.extend(part);
}

/// The accessory for a skin, fitted to the top of the head (head top is y = BY), moved down by dy.
fn accessory(skin: &str, fur: u32, dy: i32) -> Px {
    let mut a = Px::new();
    match skin {
        "headphones" => {
            a.extend(rect(10, 9, 21, 9, 0x5e5e5e)); // band over the head
            a.extend([((9, 10), 0x5e5e5e), ((22, 10), 0x5e5e5e)]);
            a.extend(rect(7, 11, 9, 13, 0x5e5e5e)); // ear cups, clear of the eyes
            a.extend(rect(22, 11, 24, 13, 0x5e5e5e));
            a.extend([((8, 12), 0xe0533d), ((23, 12), 0xe0533d)]);
        }
        "crown" => {
            a.extend(cells(&["X..XX..X", "XX.XX.XX", "XXXXXXXX"], 12, 7, 0xf5c542));
            a.extend(rect(12, 10, 19, 10, 0xd4a017));
            a.extend([((13, 10), 0x4fc3f7), ((15, 10), 0xe53935), ((16, 10), 0xe53935), ((18, 10), 0x4fc3f7)]);
        }
        "cat ears" => {
            a.extend(cells(&["X..........X", "XX........XX", "XXX......XXX"], 10, 8, fur));
            a.extend([((11, 10), 0xf4a7b9), ((20, 10), 0xf4a7b9)]); // pink inside
        }
        "beanie" => {
            a.extend(rect(15, 6, 16, 6, 0xefe6d0)); // pom-pom
            a.extend(rect(12, 7, 19, 7, 0x3f7fbf));
            a.extend(rect(11, 8, 20, 8, 0x3f7fbf));
            a.extend(rect(10, 9, 21, 10, 0x3f7fbf));
            a.extend((10..=21).map(|x| ((x, 11), if x % 2 == 1 { 0x2c5f94 } else { 0x24507d }))); // ribbed brim over the top of the head
            a.extend([((13, 8), 0x5a9ad6), ((17, 9), 0x5a9ad6), ((14, 7), 0x5a9ad6)]); // knit highlights
        }
        _ => {}
    }
    a.into_iter().map(|((x, y), c)| ((x, y + dy), c)).collect()
}

/// The pixels for this state at tick t, wearing this skin.
fn frame(state: &str, t: u32, skin: &str) -> Px {
    let mut px = Px::new();

    if state == "rip" {
        // weekly limit used up: a tombstone with a flower growing from the mud
        let stone: Px = [rect(11, 5, 19, 5, 0), rect(9, 6, 21, 6, 0), rect(8, 7, 22, 19, 0)]
            .into_iter().flatten()
            .map(|((x, y), _)| ((x, y), if x <= 9 { 0xb8bec4 } else if x >= 21 { 0x7d838a } else { 0x9aa0a6 }))
            .collect();
        layer(&mut px, &stone, true);
        layer(&mut px, &accessory(skin, SHADE[0], -5), true); // the skin rests on the tombstone
        for (i, letter) in RIP.iter().enumerate() {
            px.extend(cells(letter, 10 + 4 * i as i32, 8, 0x4a4f55));
        }
        let mut mud = rect(3, 20, 28, 20, 0x6e4a2e);
        mud.extend(rect(3, 21, 28, 22, 0x5b3a24));
        for p in [(6, 21), (11, 22), (17, 21), (23, 22), (26, 21)] {
            mud.insert(p, 0x3f2718);
        }
        layer(&mut px, &mud, true);
        let cx = 15 + ((t / 4) % 2) as i32; // flower head sways
        let mut flower = rect(15, 17, 15, 20, 0x4caf50);
        flower.extend([((14, 19), 0x66bb6a), ((16, 18), 0x66bb6a), ((cx - 1, 15), 0xf48fb1), ((cx + 1, 15), 0xf48fb1),
                       ((cx, 14), 0xf48fb1), ((cx, 16), 0xf48fb1), ((cx, 15), 0xffd54f)]);
        layer(&mut px, &flower, true);
        return px;
    }

    let rows = if state == "working" { &BODY[..8] } else { &BODY[..] }; // legs hidden behind the desk
    let mut rows: Vec<String> = rows.iter().map(|r| r.to_string()).collect();
    let mut shade = SHADE;
    if state == "scream" {
        rows = rows.iter().map(|r| format!("...{}...", &r[3..15])).collect(); // arms moved up to the face
        shade[..3].copy_from_slice(&[0x8fb3d9, 0x8fb3d9, 0xb9a2b4]); // face drains blue from the top, like 😱
    }
    let mut body = cells_by(&rows, BX, BY, |_, y| shade[(y - BY) as usize]);
    if state != "scream" {
        let shut = state == "sleeping" || (state == "idle" && t % 16 == 0); // idle blinks
        let (left, right) = if shut { (["...", "XXX", "..."], ["...", "XXX", "..."]) }
                            else { ([".XX", ".XX", "..."], ["XX.", "XX.", "..."]) };
        body.extend(cells(&left, BX + 4, BY + 3, OUT));
        body.extend(cells(&right, BX + 11, BY + 3, OUT));
    }
    layer(&mut px, &body, true);
    layer(&mut px, &accessory(skin, shade[0], 0), true);

    match state {
        "sleeping" => {
            let bob = ((t / 3) % 2) as i32;
            layer(&mut px, &cells(&ZBIG, BX + 13, BY - 7 - bob, 0xefe6d0), false); // an outline would fill the Z's gaps
            layer(&mut px, &cells(&ZSMALL, BX + 19, BY - 10 - bob, 0xefe6d0), false);
        }
        "scream" => {
            // 5h limit close: wide eyes, open mouth, hands on cheeks, shaking
            for ex in [BX + 4, BX + 11] {
                px.extend(rect(ex, BY + 1, ex + 2, BY + 3, OUT)); // wide-open eyes: dark ring, white middle
                px.insert((ex + 1, BY + 2), 0xffffff);
            }
            layer(&mut px, &cells(&[".XX.", "XXXX", ".XX."], BX + 7, BY + 6, 0x5a1f1a), true);
            let mut hands = rect(BX + 2, BY + 5, BX + 3, BY + 7, 0xf09a75);
            hands.extend(rect(BX + 14, BY + 5, BX + 15, BY + 7, 0xf09a75));
            layer(&mut px, &hands, true);
            if t % 2 == 1 {
                px = px.into_iter().map(|((x, y), c)| ((x + 1, y), c)).collect();
            }
        }
        "working" => {
            // sitting at a desk: monitor, keyboard, notes, lamp
            let mut desk = rect(2, BY + 7, 29, BY + 7, 0xb07440);
            desk.extend(rect(2, BY + 8, 29, BY + 8, 0x9a6235));
            desk.extend(rect(4, BY + 9, 5, H - 2, 0x7a4a26));
            desk.extend(rect(26, BY + 9, 27, H - 2, 0x7a4a26));
            layer(&mut px, &desk, true);
            layer(&mut px, &(11..18).map(|x| ((x, BY + 6), if x % 2 == 1 { 0x8a8a8a } else { 0x5e5e5e })).collect(), true);
            layer(&mut px, &rect(19, BY + 6, 22, BY + 6, 0xefe6d0), true);
            let mut monitor = rect(2, BY + 1, 9, BY + 5, 0x3b3330);
            monitor.extend(rect(5, BY + 6, 6, BY + 6, 0x2a2422));
            layer(&mut px, &monitor, true);
            let mut lamp = rect(25, BY + 6, 28, BY + 6, 0x3b3330);
            lamp.extend(rect(27, BY, 27, BY + 5, 0x3b3330));
            lamp.insert((26, BY - 1), 0x3b3330);
            lamp.extend(rect(22, BY - 3, 25, BY - 2, 0x3b3330));
            layer(&mut px, &lamp, true);
            px.insert((23, BY - 1), 0xffd86b); // bulb glow
            px.insert((24, BY - 1), 0xffd86b);
            let tap = (t % 2) as i32;
            let mut hands = rect(12, BY + 5 + tap, 13, BY + 5 + tap, SHADE[3]);
            hands.extend(rect(16, BY + 6 - tap, 17, BY + 6 - tap, SHADE[3]));
            layer(&mut px, &hands, true);
        }
        _ => {}
    }
    px
}

/// W*H pixels, row by row, KEY where nothing is drawn.
fn render(state: &str, t: u32, skin: &str) -> Vec<u32> {
    let mut buf = vec![KEY; (W * H) as usize];
    for ((x, y), c) in frame(state, t, skin) {
        if (0..W).contains(&x) && (0..H).contains(&y) {
            buf[(y * W + x) as usize] = c;
        }
    }
    buf
}

struct App {
    force: Option<&'static str>,
    state: &'static str,
    pcts: (f64, f64),
    source: &'static str,
    t: u32,
    usage: Usage,
    skin: &'static str,
}

fn settings_file() -> PathBuf {
    claude_dir().join("pet_settings.json")
}

/// The saved skin, or "none" if nothing (valid) is saved.
fn load_skin() -> &'static str {
    let saved = fs::read_to_string(settings_file()).ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|d| d["skin"].as_str().map(str::to_string));
    SKINS.iter().find(|&&s| Some(s) == saved.as_deref()).copied().unwrap_or("none")
}

fn save_skin(skin: &str) {
    let _ = fs::write(settings_file(), serde_json::json!({ "skin": skin }).to_string()); // losing this only forgets the skin
}

impl App {
    fn poll(&mut self) {
        let (five, week, idle) = self.usage.read();
        let real = real_limits();
        self.pcts = real.unwrap_or((100.0 * five / FIVE_H_LIMIT, 100.0 * week / WEEK_LIMIT));
        self.source = if real.is_some() { "Claude" } else { "token estimate" };
        self.state = pick(self.pcts.0, self.pcts.1, idle, claude_window());
    }
}

thread_local! {
    static APP: RefCell<App> = RefCell::new(App {
        force: None, state: "idle", pcts: (0.0, 0.0), source: "", t: 0, usage: Usage::default(), skin: "none",
    });
}

const ID_QUIT: usize = 99; // menu ids: 1 = auto, 2.. = STATES, ID_SKIN.. = SKINS
const ID_SKIN: usize = 21;

unsafe fn show_menu(h: HWND) {
    let (pcts, source, force, skin) = APP.with(|a| { let a = a.borrow(); (a.pcts, a.source, a.force, a.skin) });
    let m = CreatePopupMenu();
    let add = |menu: HMENU, flags: u32, id: usize, label: &str| { AppendMenuW(menu, flags, id, wide(label).as_ptr()); };
    add(m, MF_STRING | MF_GRAYED, 0, &format!("5h: {:.0}%   week: {:.0}%", pcts.0, pcts.1));
    add(m, MF_STRING | MF_GRAYED, 0, &format!("source: {source}"));
    AppendMenuW(m, MF_SEPARATOR, 0, ptr::null());
    for (i, s) in [None].into_iter().chain(STATES.map(Some)).enumerate() {
        add(m, MF_STRING | if s == force { MF_CHECKED } else { 0 }, i + 1, s.unwrap_or("auto"));
    }
    AppendMenuW(m, MF_SEPARATOR, 0, ptr::null());
    let skins = CreatePopupMenu(); // destroyed together with m
    for (i, s) in SKINS.iter().enumerate() {
        add(skins, MF_STRING | if *s == skin { MF_CHECKED } else { 0 }, ID_SKIN + i, s);
    }
    add(m, MF_POPUP, skins as usize, "Skin");
    AppendMenuW(m, MF_SEPARATOR, 0, ptr::null());
    add(m, MF_STRING, ID_QUIT, "Quit");
    let mut p = POINT { x: 0, y: 0 };
    GetCursorPos(&mut p);
    SetForegroundWindow(h); // so the menu closes when you click elsewhere
    // The menu runs its own message loop (timers keep ticking), so no APP borrow is held here.
    let id = TrackPopupMenu(m, TPM_RETURNCMD | TPM_RIGHTBUTTON, p.x, p.y, 0, h, ptr::null()) as usize;
    DestroyMenu(m);
    match id {
        ID_QUIT => { DestroyWindow(h); }
        1..=6 => APP.with(|a| a.borrow_mut().force = if id == 1 { None } else { Some(STATES[id - 2]) }),
        _ if (ID_SKIN..ID_SKIN + SKINS.len()).contains(&id) => {
            let s = SKINS[id - ID_SKIN];
            APP.with(|a| a.borrow_mut().skin = s);
            save_skin(s);
        }
        _ => {}
    }
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER => {
            APP.with(|a| {
                let mut a = a.borrow_mut();
                if wp == 1 { a.t += 1; } else { a.poll(); }
            });
            InvalidateRect(h, ptr::null(), 0);
            0
        }
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(h, &mut ps);
            let buf = APP.with(|a| { let a = a.borrow(); render(a.force.unwrap_or(a.state), a.t, a.skin) });
            let mut bi: BITMAPINFO = std::mem::zeroed();
            bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bi.bmiHeader.biWidth = W;
            bi.bmiHeader.biHeight = -H; // top-down rows
            bi.bmiHeader.biPlanes = 1;
            bi.bmiHeader.biBitCount = 32;
            bi.bmiHeader.biCompression = BI_RGB as _;
            StretchDIBits(hdc, 0, 0, W * S, H * S, 0, 0, W, H, buf.as_ptr().cast(), &bi, DIB_RGB_COLORS, SRCCOPY);
            EndPaint(h, &ps);
            0
        }
        WM_ERASEBKGND => 1, // WM_PAINT covers every pixel; skipping the erase avoids flicker
        WM_LBUTTONDOWN => {
            ReleaseCapture(); // drag the window by any visible pixel
            SendMessageW(h, WM_NCLBUTTONDOWN, HTCAPTION as WPARAM, 0);
            0
        }
        WM_RBUTTONUP => { show_menu(h); 0 }
        WM_DESTROY => { PostQuitMessage(0); 0 }
        _ => DefWindowProcW(h, msg, wp, lp),
    }
}

fn run() {
    unsafe {
        let inst = GetModuleHandleW(ptr::null());
        let class = wide("ClaudePixelPet");
        if !FindWindowW(class.as_ptr(), ptr::null()).is_null() {
            return; // already on the desktop (e.g. `npx claude-pixel-pet` run twice)
        }
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = inst;
        wc.hCursor = LoadCursorW(ptr::null_mut(), IDC_ARROW);
        wc.lpszClassName = class.as_ptr();
        RegisterClassW(&wc);
        let (w, h) = (W * S, H * S);
        let (x, y) = (GetSystemMetrics(SM_CXSCREEN) - w - 40, GetSystemMetrics(SM_CYSCREEN) - h - 70);
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW, // toolwindow = no taskbar button
            class.as_ptr(), wide("Pixel Pet").as_ptr(), WS_POPUP | WS_VISIBLE,
            x, y, w, h, ptr::null_mut(), ptr::null_mut(), inst, ptr::null(),
        );
        SetLayeredWindowAttributes(hwnd, KEY, 0, LWA_COLORKEY);
        APP.with(|a| { let mut a = a.borrow_mut(); a.skin = load_skin(); a.poll(); });
        SetTimer(hwnd, 1, 250, None); // animation tick
        SetTimer(hwnd, 2, 3000, None); // usage poll
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Writes the README GIFs: pet_preview.gif (every mood) and pet_skins.gif (every mood in each skin).
fn preview() -> Result<(), Box<dyn std::error::Error>> {
    write_gif("pet_preview.gif", &SKINS[..1])?;
    write_gif("pet_skins.gif", &SKINS[1..])
}

/// One animated GIF: moods as columns, the given skins as rows.
fn write_gif(path: &str, skins: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    let (cw, ch) = ((W * S) as usize, (H * S) as usize);
    let (fw, fh) = (cw * STATES.len(), ch * skins.len());
    let mut enc = gif::Encoder::new(fs::File::create(path)?, fw as u16, fh as u16, &[])?;
    enc.set_repeat(gif::Repeat::Infinite)?;
    for t in 0..48 {
        // 48 ticks = 12 s, a whole cycle of every animation (blink 16, Zz 6, flower 8)
        let mut rgb = vec![0u8; fw * fh * 3];
        for (row, skin) in skins.iter().enumerate() {
            for (col, s) in STATES.iter().enumerate() {
                let buf = render(s, t, skin);
                for py in 0..ch {
                    for px in 0..cw {
                        let c = buf[(py / S as usize) * W as usize + px / S as usize];
                        let c = if c == KEY { 0x28201c } else { c };
                        let o = ((row * ch + py) * fw + col * cw + px) * 3;
                        rgb[o..o + 3].copy_from_slice(&[(c >> 16) as u8, (c >> 8) as u8, c as u8]);
                    }
                }
            }
        }
        let mut f = gif::Frame::from_rgb_speed(fw as u16, fh as u16, &rgb, 10);
        f.delay = 25; // centiseconds = 250 ms, the app's tick
        enc.write_frame(&f)?;
    }
    Ok(())
}

fn main() {
    if std::env::args().any(|a| a == "--preview") {
        if let Err(e) = preview() {
            let text = wide(&format!("Couldn't write the preview GIFs: {e}"));
            unsafe { MessageBoxW(ptr::null_mut(), text.as_ptr(), wide("Pixel Pet").as_ptr(), MB_ICONERROR) };
        }
    } else {
        run();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moods() {
        assert_eq!(pick(0.0, 100.0, 0.0, "open"), "rip");
        assert_eq!(pick(80.0, 0.0, 0.0, "open"), "scream");
        assert_eq!(pick(79.0, 99.0, 0.0, "open"), "working");
        assert_eq!(pick(0.0, 0.0, 10.0, "open"), "working");
        assert_eq!(pick(0.0, 0.0, 9999.0, "open"), "sleeping");
        assert_eq!(pick(0.0, 0.0, 300.0, "open"), "idle");
        assert_eq!(pick(0.0, 0.0, 10.0, "minimized"), "sleeping");
        assert_eq!(pick(0.0, 0.0, 10.0, "closed"), "idle");
        assert_eq!(pick(90.0, 0.0, 10.0, "closed"), "scream");
    }

    #[test]
    fn frames_fit_the_grid() {
        for skin in SKINS {
            for s in STATES {
                for t in 0..48 {
                    let f = frame(s, t, skin);
                    assert!(f.keys().all(|&(x, y)| (0..W).contains(&x) && (0..H).contains(&y)), "{skin} {s} t={t}");
                }
            }
        }
    }

    #[test]
    fn no_skin_draws_exactly_what_it_did_before() {
        for s in STATES {
            for t in 0..48 {
                assert_eq!(frame(s, t, "none"), frame(s, t, "unknown skin"), "{s}");
            }
        }
        assert!(SKINS[1..].iter().all(|k| !accessory(k, 0, 0).is_empty()));
    }

    #[test]
    fn timestamps() {
        assert_eq!(parse_ts("1970-01-01T00:00:00Z"), Some(0.0));
        assert_eq!(parse_ts("2000-03-01T00:00:00.500Z"), Some(951_868_800.5));
        assert_eq!(parse_ts("2026-11-14T12:00:00+02:00"), parse_ts("2026-11-14T10:00:00Z"));
        assert_eq!(parse_ts("nonsense"), None);
    }

    #[test]
    fn live_readout() {
        // Not an assert: prints what the pet sees right now (cargo test -- --nocapture).
        println!("Claude window: {} | real limits: {:?}", claude_window(), real_limits());
        println!("usage (5h, week, idle secs): {:?}", Usage::default().read());
    }
}
