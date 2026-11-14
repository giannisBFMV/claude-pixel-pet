"""Desktop pixel pet that reacts to your Claude Code usage.
Run:  pythonw pixel_pet.py      (drag with left mouse, right-click for menu)
Test: python pixel_pet.py --test
"""
import glob, json, os, sys, time, tkinter as tk
from datetime import datetime

# Real limits come from LIMITS_FILE, written by the Claude Code status line script.
# Without it, usage is estimated from tokens (input + output + cache writes) against these:
FIVE_H_LIMIT = 8_000_000
WEEK_LIMIT = 80_000_000
SCREAM_PCT = 80        # % of the 5h limit where it starts screaming
LIMITS_FILE = os.path.expanduser("~/.claude/pet_limits.json")
WORKING_SECS = 90      # a transcript written this recently -> typing
SLEEP_SECS = 600       # no activity this long -> sleeping
PROJECTS = os.path.expanduser("~/.claude/projects")

W, H, S = 32, 26, 5    # grid cells, cell size in px
KEY = "#ff00ff"        # transparent color
OUT = "#1c120e"        # outline + eyes
BODY = ["...XXXXXXXXXXXX...",
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
        "...XX.XX..XX.XX..."]
SHADE = ["#ec8a62"] * 2 + ["#d97757"] * 6 + ["#b95d3f"] * 4   # color per body row
BX, BY = 7, 11         # body offset in the grid
EYES = {  # (left, right) eye, 3x3 each, placed at body cols 4 and 11, row 3
    "open":  ([".XX", ".XX", "..."], ["XX.", "XX.", "..."]),
    "shut":  (["...", "XXX", "..."], ["...", "XXX", "..."]),
}
ZBIG = ["XXXXX", "...X.", "..X..", ".X...", "XXXXX"]
ZSMALL = ["XXXX", "..X.", ".X..", "XXXX"]


def cells(rows, x0, y0, color):
    return {(x0 + x, y0 + y): color for y, r in enumerate(rows) for x, c in enumerate(r) if c == "X"}


def rect(x0, y0, x1, y1, color):
    return {(x, y): color for x in range(x0, x1 + 1) for y in range(y0, y1 + 1)}


def pick(five_pct, week_pct, idle, app="open"):
    if week_pct >= 100: return "rip"
    if five_pct >= SCREAM_PCT: return "scream"
    if app == "closed": return "idle"
    if app == "minimized": return "sleeping"
    if idle < WORKING_SECS: return "working"
    if idle > SLEEP_SECS: return "sleeping"
    return "idle"


import ctypes
from ctypes import wintypes
u32, k32 = ctypes.windll.user32, ctypes.windll.kernel32
k32.CreateToolhelp32Snapshot.restype = wintypes.HANDLE


class PE(ctypes.Structure):
    _fields_ = [("dwSize", wintypes.DWORD), ("cntUsage", wintypes.DWORD), ("pid", wintypes.DWORD),
                ("heap", ctypes.c_size_t), ("mod", wintypes.DWORD), ("threads", wintypes.DWORD),
                ("ppid", wintypes.DWORD), ("prio", ctypes.c_long), ("flags", wintypes.DWORD),
                ("exe", ctypes.c_wchar * 260)]


def processes():
    snap = k32.CreateToolhelp32Snapshot(2, 0)  # TH32CS_SNAPPROCESS
    e, out = PE(), {}
    e.dwSize = ctypes.sizeof(PE)
    ok = k32.Process32FirstW(snap, ctypes.byref(e))
    while ok:
        out[e.pid] = (e.ppid, e.exe.lower())
        ok = k32.Process32NextW(snap, ctypes.byref(e))
    k32.CloseHandle(snap)
    return out


def windows():
    """pid -> [is_minimized, ...] for visible, titled, top-level windows."""
    out = {}
    @ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
    def cb(h, _):
        if u32.IsWindowVisible(h) and u32.GetWindowTextLengthW(h) and not u32.GetWindow(h, 4):  # 4 = GW_OWNER
            pid = wintypes.DWORD()
            u32.GetWindowThreadProcessId(h, ctypes.byref(pid))
            out.setdefault(pid.value, []).append(bool(u32.IsIconic(h)))
        return True
    u32.EnumWindows(cb, 0)
    return out


def claude_window():
    """'open', 'minimized' or 'closed' for the Claude app and any terminal running the claude CLI."""
    # shortcut: the CLI is found as claude.exe and its window via parent processes (terminal, VS Code);
    # an npm/node-installed CLI isn't detected, match node's command line if you need it.
    procs, wins, found = processes(), windows(), []
    for pid, (_, name) in procs.items():
        if name != "claude.exe": continue
        seen = set()
        while pid in procs and pid not in seen and procs[pid][1] != "explorer.exe":
            seen.add(pid)
            if pid in wins:
                found += wins[pid]
                break
            pid = procs[pid][0]
    if not found: return "closed"
    return "minimized" if all(found) else "open"


def real_limits():
    """(5h %, week %) as Claude reported them to the status line, or None if not saved yet."""
    try:
        with open(LIMITS_FILE, encoding="utf-8-sig") as f:
            d = json.load(f)
        now = time.time()
        pct = lambda w: w.get("used_percentage") or 0 if w and w.get("resets_at", now + 1) > now else 0  # past reset = 0
        return pct(d.get("five_hour")), pct(d.get("seven_day"))
    except (OSError, ValueError, AttributeError):
        return None


_cache = {}
def entries(path):
    key = (os.path.getmtime(path), os.path.getsize(path))
    if path not in _cache or _cache[path][0] != key:
        out = []
        with open(path, encoding="utf-8", errors="ignore") as f:
            for line in f:
                if '"usage"' not in line: continue
                try:
                    d = json.loads(line)
                    m, u = d["message"], d["message"]["usage"]
                    ts = datetime.fromisoformat(d["timestamp"].replace("Z", "+00:00")).timestamp()
                except (ValueError, KeyError, TypeError):
                    continue
                tok = sum(u.get(k) or 0 for k in ("input_tokens", "output_tokens", "cache_creation_input_tokens"))
                out.append((m.get("id") or d.get("uuid"), ts, tok))
        _cache[path] = (key, out)
    return _cache[path][1]


def read_usage():
    # shortcut: rolling 5h/7d token windows, not Anthropic's exact reset times; tune limits above.
    now, seen, last = time.time(), {}, 0
    for p in glob.glob(os.path.join(PROJECTS, "**", "*.jsonl"), recursive=True):
        try:
            mt = os.path.getmtime(p)
            if now - mt > 7 * 86400: continue
            last = max(last, mt)
            for mid, ts, tok in entries(p): seen[mid] = (ts, tok)  # last line per message id wins
        except OSError:
            continue
    five = sum(t for ts, t in seen.values() if now - ts < 5 * 3600)
    week = sum(t for ts, t in seen.values() if now - ts < 7 * 86400)
    return five, week, now - last


RIP = {"R": ["XX.", "X.X", "XX.", "X.X", "X.X"],
       "I": ["XXX", ".X.", ".X.", ".X.", "XXX"],
       "P": ["XX.", "X.X", "XX.", "X..", "X.."]}


def frame(state, t):
    """Return {(x, y): color} for this state at tick t."""
    px = {}

    def layer(part, edge=True):  # paint back to front; each part gets its own dark outline
        if edge:
            for x, y in part:
                for dx in (-1, 0, 1):
                    for dy in (-1, 0, 1):
                        if (x + dx, y + dy) not in part: px[(x + dx, y + dy)] = OUT
        px.update(part)

    if state == "rip":  # weekly limit used up: a tombstone with a flower growing from the mud
        stone = {**rect(11, 5, 19, 5, ""), **rect(9, 6, 21, 6, ""), **rect(8, 7, 22, 19, "")}
        layer({(x, y): "#b8bec4" if x <= 9 else "#7d838a" if x >= 21 else "#9aa0a6" for x, y in stone})
        for i, ch in enumerate("RIP"):
            px.update(cells(RIP[ch], 10 + 4 * i, 8, "#4a4f55"))
        mud = {**rect(3, 20, 28, 20, "#6e4a2e"), **rect(3, 21, 28, 22, "#5b3a24")}
        mud.update({p: "#3f2718" for p in ((6, 21), (11, 22), (17, 21), (23, 22), (26, 21))})
        layer(mud)
        cx = 15 + (t // 4) % 2  # flower head sways
        layer({**rect(15, 17, 15, 20, "#4caf50"), (14, 19): "#66bb6a", (16, 18): "#66bb6a",
               (cx - 1, 15): "#f48fb1", (cx + 1, 15): "#f48fb1", (cx, 14): "#f48fb1", (cx, 16): "#f48fb1",
               (cx, 15): "#ffd54f"})
        return px

    rows = BODY[:8] if state == "working" else BODY  # legs hidden behind the desk
    shade = SHADE
    if state == "scream":
        rows = ["..." + r[3:15] + "..." for r in rows]  # arms moved up to the face
        shade = ["#8fb3d9"] * 2 + ["#b9a2b4"] + SHADE[3:]  # face drains blue from the top, like 😱
    body = {p: shade[p[1] - BY] for p in cells(rows, BX, BY, None)}
    if state != "scream":
        eye = "shut" if state == "sleeping" or (state == "idle" and t % 16 == 0) else "open"  # idle blinks
        left, right = EYES[eye]
        body.update(cells(left, BX + 4, BY + 3, OUT))
        body.update(cells(right, BX + 11, BY + 3, OUT))
    layer(body)

    if state == "sleeping":
        bob = (t // 3) % 2
        layer(cells(ZBIG, BX + 13, BY - 7 - bob, "#efe6d0"), edge=False)  # an outline would fill the Z's gaps
        layer(cells(ZSMALL, BX + 19, BY - 10 - bob, "#efe6d0"), edge=False)
    elif state == "scream":  # 5h limit close: wide eyes, open mouth, hands on cheeks, shaking
        for ex in (BX + 4, BX + 11):  # wide-open eyes: dark ring, white middle
            px.update({**rect(ex, BY + 1, ex + 2, BY + 3, OUT), (ex + 1, BY + 2): "#ffffff"})
        layer(cells([".XX.", "XXXX", ".XX."], BX + 7, BY + 6, "#5a1f1a"))
        layer({**rect(BX + 2, BY + 5, BX + 3, BY + 7, "#f09a75"), **rect(BX + 14, BY + 5, BX + 15, BY + 7, "#f09a75")})
        if t % 2:
            px = {(x + 1, y): c for (x, y), c in px.items()}
    elif state == "working":  # sitting at a desk: monitor, keyboard, notes, lamp
        layer({**rect(2, BY + 7, 29, BY + 7, "#b07440"), **rect(2, BY + 8, 29, BY + 8, "#9a6235"),
               **rect(4, BY + 9, 5, H - 2, "#7a4a26"), **rect(26, BY + 9, 27, H - 2, "#7a4a26")})
        layer({(x, BY + 6): "#8a8a8a" if x % 2 else "#5e5e5e" for x in range(11, 18)})
        layer(rect(19, BY + 6, 22, BY + 6, "#efe6d0"))
        layer({**rect(2, BY + 1, 9, BY + 5, "#3b3330"), **rect(5, BY + 6, 6, BY + 6, "#2a2422")})
        layer({**rect(25, BY + 6, 28, BY + 6, "#3b3330"), **rect(27, BY, 27, BY + 5, "#3b3330"),
               (26, BY - 1): "#3b3330", **rect(22, BY - 3, 25, BY - 2, "#3b3330")})
        px[(23, BY - 1)] = px[(24, BY - 1)] = "#ffd86b"  # bulb glow
        tap = t % 2
        layer({**rect(12, BY + 5 + tap, 13, BY + 5 + tap, SHADE[3]), **rect(16, BY + 6 - tap, 17, BY + 6 - tap, SHADE[3])})
    return px


class Pet:
    def __init__(self):
        self.root = r = tk.Tk()
        r.overrideredirect(True)
        r.attributes("-topmost", True)
        r.config(bg=KEY)
        r.attributes("-transparentcolor", KEY)
        r.geometry(f"+{r.winfo_screenwidth() - W * S - 40}+{r.winfo_screenheight() - H * S - 70}")
        self.cv = tk.Canvas(r, width=W * S, height=H * S, bg=KEY, highlightthickness=0)
        self.cv.pack()
        self.cv.bind("<ButtonPress-1>", lambda e: setattr(self, "drag", (e.x, e.y)))
        self.cv.bind("<B1-Motion>", lambda e: r.geometry(f"+{e.x_root - self.drag[0]}+{e.y_root - self.drag[1]}"))
        self.cv.bind("<Button-3>", self.menu)
        self.force, self.state, self.pcts, self.source, self.t = None, "idle", (0, 0), "", 0
        self.poll(); self.tick()
        r.mainloop()

    def menu(self, e):
        m = tk.Menu(self.root, tearoff=0)
        m.add_command(label=f"5h: {self.pcts[0]:.0f}%   week: {self.pcts[1]:.0f}%", state="disabled")
        m.add_command(label=f"source: {self.source}", state="disabled")
        m.add_separator()
        for s in (None,) + STATES:
            m.add_radiobutton(label=s or "auto", command=lambda s=s: setattr(self, "force", s))
        m.add_separator()
        m.add_command(label="Quit", command=self.root.destroy)
        m.tk_popup(e.x_root, e.y_root)

    def poll(self):
        five, week, idle = read_usage()
        real = real_limits()
        self.pcts = real or (100 * five / FIVE_H_LIMIT, 100 * week / WEEK_LIMIT)
        self.source = "Claude" if real else "token estimate"
        self.state = pick(*self.pcts, idle, claude_window())
        self.root.after(3000, self.poll)

    def tick(self):
        self.t += 1
        self.cv.delete("all")
        for (x, y), c in frame(self.force or self.state, self.t).items():
            self.cv.create_rectangle(x * S, y * S, x * S + S, y * S + S, fill=c, width=0)
        self.root.after(250, self.tick)


STATES = ("idle", "working", "sleeping", "scream", "rip")

if __name__ == "__main__":
    if "--test" in sys.argv:
        assert pick(0, 100, 0) == "rip"
        assert pick(80, 0, 0) == "scream"
        assert pick(79, 99, 0) == "working"
        assert pick(0, 0, 10) == "working"
        assert pick(0, 0, 9999) == "sleeping"
        assert pick(0, 0, 300) == "idle"
        assert pick(0, 0, 10, "minimized") == "sleeping"
        assert pick(0, 0, 10, "closed") == "idle"
        assert pick(90, 0, 10, "closed") == "scream"
        print("Claude window:", claude_window(), "| real limits:", real_limits())
        for s in STATES:
            assert all(0 <= x < W and 0 <= y < H for x, y in frame(s, 3)), s
        print("ok; current usage (5h, week, idle secs):", read_usage())
    elif "--preview" in sys.argv:  # writes pet_preview.gif for the README (needs: pip install pillow)
        from PIL import Image, ImageDraw
        frames = []
        for t in range(48):  # 48 ticks = 12 s, a whole cycle of every animation (blink 16, Zz 6, flower 8)
            img = Image.new("RGB", (W * S * len(STATES), H * S), "#28201c")
            d = ImageDraw.Draw(img)
            for i, s in enumerate(STATES):
                for (x, y), c in frame(s, t).items():
                    d.rectangle([(i * W + x) * S, y * S, (i * W + x + 1) * S - 1, (y + 1) * S - 1], fill=c)
            frames.append(img)
        frames[0].save("pet_preview.gif", save_all=True, append_images=frames[1:], duration=250, loop=0)
        print("wrote pet_preview.gif")
    else:
        Pet()
