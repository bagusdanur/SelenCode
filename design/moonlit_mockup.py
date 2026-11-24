#!/usr/bin/env python3
"""Mockup desain TUI SelenCode 'Moonlit' v2 — bulan digambar sebagai titik (dots)."""
from PIL import Image, ImageDraw, ImageFont

FONT = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"
FONTB = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Bold.ttf"
SIZE = 25
CW = 15.05
LH = 30

BG = (13, 10, 26)
VIOLET = (167, 139, 250)
DEEP = (109, 40, 217)
LAV = (196, 181, 253)
SILVER = (229, 231, 235)
DIM = (148, 140, 180)
FAINT = (94, 84, 130)
GOLD = (252, 211, 77)


def moon_dots(w_px, h_px, dot=9.0, gap=15.0):
    """Crescent dari titik-titik ungu: lingkaran besar minus lingkaran geser."""
    img = Image.new("RGBA", (int(w_px), int(h_px)), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    cx, cy, r = w_px * 0.44, h_px * 0.52, h_px * 0.47
    cx2, cy2, r2 = cx + w_px * 0.36, cy - h_px * 0.12, r * 0.94
    y = gap / 2
    row = 0
    while y < h_px:
        x = gap / 2 + (gap / 2 if row % 2 else 0)
        while x < w_px:
            inside = (x - cx) ** 2 + (y - cy) ** 2 <= r * r
            cut = (x - cx2) ** 2 + (y - cy2) ** 2 <= r2 * r2
            if inside and not cut:
                rr = dot / 2
                d.ellipse([x - rr, y - rr, x + rr, y + rr], fill=VIOLET + (255,))
            x += gap
        y += gap
        row += 1
    # bintang kecil dekat ujung atas sabit
    sx, sy = cx + r * 0.62, cy - r * 0.78
    L = dot * 0.95
    d.polygon([(sx, sy - L), (sx + L * 0.28, sy - L * 0.28), (sx + L, sy),
               (sx + L * 0.28, sy + L * 0.28), (sx, sy + L),
               (sx - L * 0.28, sy + L * 0.28), (sx - L, sy),
               (sx - L * 0.28, sy - L * 0.28)], fill=GOLD + (255,))
    return img


PIX = {
    "S": ["01110", "10001", "10000", "01110", "00001", "10001", "01110"],
    "E": ["11111", "10000", "10000", "11110", "10000", "10000", "11111"],
    "L": ["10000", "10000", "10000", "10000", "10000", "10000", "11111"],
    "N": ["10001", "11001", "10101", "10011", "10001", "10001", "10001"],
    "C": ["01110", "10001", "10000", "10000", "10000", "10001", "01110"],
    "O": ["01110", "10001", "10001", "10001", "10001", "10001", "01110"],
    "D": ["11110", "10001", "10001", "10001", "10001", "10001", "11110"],
}


def wordmark(word):
    rows = [""] * 7
    for ch in word:
        g = PIX[ch]
        for i in range(7):
            rows[i] += g[i].replace("1", "█").replace("0", " ") + " "
    return rows


class Screen:
    def __init__(self, cols):
        self.cols = cols
        self.lines = []
        self.pastes = []  # (img, char_x, line_y)

    def add(self, spans):
        if isinstance(spans, str):
            spans = [(spans, SILVER, False)]
        self.lines.append(spans)

    def render(self, path):
        w = int(self.cols * CW) + 28
        h = len(self.lines) * LH + 28
        img = Image.new("RGB", (w, h), BG)
        d = ImageDraw.Draw(img)
        f = ImageFont.truetype(FONT, SIZE)
        fb = ImageFont.truetype(FONTB, SIZE)
        y = 14
        for spans in self.lines:
            x = 14.0
            for text, color, bold in spans:
                d.text((x, y), text, font=fb if bold else f, fill=color)
                x += len(text) * CW
            y += LH
        for im, cx, ly in self.pastes:
            img.paste(im, (int(14 + cx * CW), int(14 + ly * LH)), im)
        img.save(path)
        print(path, img.size)


def row(s, left_spans, right_spans=None, right_start=None):
    lt = "".join(t for t, _, _ in left_spans)
    spans = [("│ ", FAINT, False)] + left_spans
    cur = 2 + len(lt)
    if right_spans is not None:
        rt = "".join(t for t, _, _ in right_spans)
        target = right_start if right_start is not None else s.cols - 2 - len(rt)
        spans.append((" " * max(target - cur, 1), SILVER, False))
        spans += right_spans
        cur = target + len(rt)
    spans.append((" " * max(s.cols - 2 - cur, 0), SILVER, False))
    spans.append(("│", FAINT, False))
    s.add(spans)


def blank(s):
    row(s, [("", SILVER, False)])


def wide():
    s = Screen(116)
    mark = wordmark("SELENCODE")
    s.add([("╭" + "─" * (s.cols - 2) + "╮", FAINT, False)])
    row(s, [("  ", SILVER, False)], [("SelenCode v0.1.1 ", VIOLET, True), ("· edisi Moonlit", DIM, False)])
    info = [
        [("  ", SILVER, False)],
        [(mark[0], VIOLET, True)],
        [(mark[1], VIOLET, True)],
        [(mark[2], VIOLET, True)],
        [(mark[3], LAV, True)],
        [(mark[4], LAV, True)],
        [(mark[5], VIOLET, True)],
        [(mark[6], VIOLET, True)],
        [("  ", SILVER, False)],
        [("  terminal AI coding agent · full Rust · Termux & Linux", DIM, False)],
        [("  ", SILVER, False)],
        [("  Selena", LAV, True), ("  menemanimu malam ini — sesi baru, konteks bersih.", SILVER, False)],
        [("  Provider  ", DIM, False), ("9Router (router.ryukomik.com)", SILVER, False), ("   Model  ", DIM, False), ("ag/gemini-3-flash", LAV, True)],
    ]
    for i in range(13):
        left = [("  ", SILVER, False)]
        right = info[i]
        spans = [("│ ", FAINT, False)] + left
        cur = 2 + 2
        spans.append((" " * (32 - cur), SILVER, False))
        spans += right
        total = sum(len(t) for t, _, _ in spans)
        spans.append((" " * max(s.cols - total, 0), SILVER, False))
        spans.append(("│", FAINT, False))
        s.add(spans)
    s.pastes.append((moon_dots(27 * CW, 12.4 * LH), 3.5, 2.2))
    blank(s)
    row(s, [("  ✦ Tools", GOLD, True)], [("✦ Perintah", GOLD, True)], right_start=60)
    tools = [
        ("file    ", "list_files, read_file, write_file, edit_file"),
        ("cari    ", "search_text, outline (struktur kode)"),
        ("shell   ", "run_shell (izin dulu)"),
        ("git     ", "git_status, git_diff, git_commit"),
        ("web     ", "fetch_url"),
        ("rencana ", "update_plan, delegate (subagent paralel)"),
        ("mcp     ", "mcp__* dari server eksternalmu"),
    ]
    cmds = [
        ("/plan     ", "rencana dulu, eksekusi sesudah disetujui"),
        ("/model    ", "lihat & ganti provider/model"),
        ("/sessions ", "daftar sesi tersimpan"),
        ("/resume   ", "lanjutkan sesi terakhir"),
        ("/help     ", "semua perintah"),
        ("", ""),
        ("", "12 tool aktif · sandbox: folder ini saja"),
    ]
    for (tk, tv), (ck, cv) in zip(tools, cmds):
        left = [("  " + tk, DEEP, True), (tv[:48], SILVER, False)]
        right = [(ck, DEEP, True), (cv, SILVER, False)]
        row(s, left, right, right_start=60)
    blank(s)
    row(s, [("  Selamat datang kembali. Ketik pesanmu, atau /help untuk perintah.", SILVER, False)])
    s.add([("╰" + "─" * (s.cols - 2) + "╯", FAINT, False)])
    s.add([("╭" + "─" * (s.cols - 2) + "╮", DEEP, False)])
    row_txt = "│ " + "❯ " + "Tanya apa saja…  (Enter kirim · Alt+Enter baris baru)"
    s.add([(row_txt, DIM, False), (" " * (s.cols - len(row_txt) - 1), SILVER, False), ("│", DEEP, False)])
    s.add([("╰" + "─" * (s.cols - 2) + "╯", DEEP, False)])
    s.add([
        ("  ◐ konteks ", VIOLET, True),
        ("▓▓▓▓▓▓░░░░", DEEP, False),
        (" 43%  ·  ", DIM, False),
        ("token sesi 12.480  ·  ", DIM, False),
        ("langkah 3  ·  ", DIM, False),
        ("~/project-kamu", LAV, False),
        ("   ✦ ", GOLD, False),
        ("Enter kirim · Ctrl+C keluar", FAINT, False),
    ])
    s.render("/home/hatch/workspace/termux-agent-rust/design/mockup-moonlit-wide.png")


def phone():
    s = Screen(52)
    s.add([("╭" + "─" * (s.cols - 2) + "╮", FAINT, False)])
    for i in range(8):
        spans = [("│ ", FAINT, False)]
        if i == 3:
            spans.append((" " * 22, SILVER, False))
            spans.append(("SELENCODE", LAV, True))
        elif i == 4:
            spans.append((" " * 22, SILVER, False))
            spans.append(("v0.1.1 · Moonlit", DIM, False))
        total = sum(len(t) for t, _, _ in spans)
        spans.append((" " * max(s.cols - total, 0), SILVER, False))
        spans.append(("│", FAINT, False))
        s.add(spans)
    s.pastes.append((moon_dots(17 * CW, 7.6 * LH), 4, 1.2))
    blank(s)
    row(s, [("  Selena", LAV, True), (" menemanimu malam ini.", SILVER, False)])
    row(s, [("  Provider ", DIM, False), ("9Router", SILVER, False), ("  ·  ", DIM, False), ("ag/gemini-3-flash", LAV, True)])
    row(s, [("  Folder   ", DIM, False), ("~/project-kamu", SILVER, False)])
    row(s, [("  Sesi     ", DIM, False), ("baru · 12 tool aktif · sandbox aktif", SILVER, False)])
    blank(s)
    row(s, [("  ✦ /plan", GOLD, True), (" rencana dulu   ", SILVER, False), ("✦ /model", GOLD, True), (" ganti model", SILVER, False)])
    row(s, [("  ✦ /help", GOLD, True), (" semua perintah", SILVER, False)])
    s.add([("╰" + "─" * (s.cols - 2) + "╯", FAINT, False)])
    s.add([("╭" + "─" * (s.cols - 2) + "╮", DEEP, False)])
    row_txt = "│ ❯ Tanya apa saja…"
    s.add([(row_txt, DIM, False), (" " * (s.cols - len(row_txt) - 1), SILVER, False), ("│", DEEP, False)])
    s.add([("╰" + "─" * (s.cols - 2) + "╯", DEEP, False)])
    s.add([
        (" ◐ ", VIOLET, True),
        ("▓▓▓▓▓▓░░░░", DEEP, False),
        (" 43% · 12.480 token", DIM, False),
    ])
    s.render("/home/hatch/workspace/termux-agent-rust/design/mockup-moonlit-hp.png")


wide()
phone()
