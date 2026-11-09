"""Page-by-page acceptance report (REPORT.md + thumbnails) from `compare` output.

usage: acceptance-report.py <compare.txt> <actual-dir> <expected-dir> <diff-dir> <report-dir>

compare.txt must come from `IWORK2PDF_PAGES=1 iwork2pdf compare ... --diff-dir <diff-dir>`.
Needs Pillow and poppler (pdftoppm, pdftotext).
"""
import html
import math
import re
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from urllib.parse import quote

from PIL import Image

Image.MAX_IMAGE_PIXELS = None  # our own diff PNGs of very tall Numbers pages exceed Pillow's cap

compare_txt, actual_dir, expected_dir, diff_dir, report_dir = map(Path, sys.argv[1:6])
img_dir = report_dir / "img"
img_dir.mkdir(parents=True, exist_ok=True)
THUMB = 280  # px, longest side

LINE = re.compile(r"^(PASS|FAIL|ERROR|MISSING|SKIP)\s+(\d+)\s+([\d.]+)\s+([\d.]+)\s+(\d+)\s+(\S.*?)(?:\s+--\s+(.*))?$")
docs = []
for line in compare_txt.read_text().splitlines():
    m = LINE.match(line)
    if not m:
        continue
    verdict, pages, layout, text, _, name, detail = m.groups()
    detail = detail or ""
    page_scores = {}
    pm = re.search(r"pages (.*)$", detail)
    if pm:
        for item in pm.group(1).split(", "):
            n, lt = item.split(" ")
            if lt != "n/a":
                lay, txt = lt.split("/")
                page_scores[int(n)] = (float(lay), float(txt))
        detail = detail[: pm.start()].rstrip("; ")
    docs.append(dict(verdict=verdict, pages=int(pages), name=name, detail=detail, scores=page_scores))

def slug(name: str) -> str:
    return re.sub(r"[^A-Za-z0-9._-]", "_", name.removesuffix(".pdf"))


def thumbs(pdf: Path, prefix: Path) -> None:
    if pdf.exists() and not list(prefix.parent.glob(prefix.name + "-*.jpg")):
        subprocess.run(["pdftoppm", "-jpeg", "-jpegopt", "quality=70", "-scale-to", str(THUMB),
                        str(pdf), str(prefix)], capture_output=True)


def words(pdf: Path) -> dict[int, list[tuple[str, float, float]]]:
    """Per page: (word, xMin, yMin) from pdftotext -bbox."""
    if not pdf.exists():
        return {}
    out = subprocess.run(["pdftotext", "-bbox", str(pdf), "-"], capture_output=True, text=True,
                         errors="replace").stdout
    pages, page = {}, 0
    for line in out.splitlines():
        if "<page " in line:
            page += 1
            pages[page] = []
        m = re.search(r'xMin="([\d.]+)" yMin="([\d.]+)"[^>]*>(.*)</word>', line)
        if m and page:
            pages[page].append((html.unescape(m.group(3)), float(m.group(1)), float(m.group(2))))
    return pages


def text_issue(ours, theirs) -> str:
    pool: dict[str, list[list]] = {}
    for w, x, y in ours:
        pool.setdefault(w, []).append([x, y, False])
    missing, moved = [], []
    for w, x, y in theirs:
        best = None
        for cand in pool.get(w, []):
            if not cand[2]:
                d = math.hypot(cand[0] - x, cand[1] - y)
                if best is None or d < best[0]:
                    best = (d, cand)
        if best is None:
            missing.append(w)
            continue
        best[1][2] = True
        if best[0] > 1:
            moved.append((best[1][0] - x, best[1][1] - y))
    extra = [w for w, cands in pool.items() for c in cands if not c[2]]
    parts = []
    if moved:
        dxs = sorted(abs(d[0]) for d in moved)
        dys = sorted(abs(d[1]) for d in moved)
        med = lambda v: v[len(v) // 2]
        axis = "horizontally" if med(dxs) > med(dys) else "vertically"
        parts.append(f"{len(moved)}/{len(theirs)} words shifted {axis} (median {max(med(dxs), med(dys)):.1f}pt, max {max(max(dxs), max(dys)):.0f}pt)")
    fmt = lambda ws: ", ".join(f"`{w[:20].replace('|', '/').replace('`', '')}`" for w in ws[:4]) + (" ..." if len(ws) > 4 else "")
    if missing:
        parts.append(f"{len(missing)} words missing ({fmt(missing)})")
    if extra:
        parts.append(f"{len(extra)} extra words ({fmt(extra)})")
    return "; ".join(parts) or "glyph positions/colours differ within words (spacing or colour)"


GRID = [["top-left", "top", "top-right"], ["left", "centre", "right"], ["bottom-left", "bottom", "bottom-right"]]


def layout_issue(diff_png: Path, diff_jpg: Path) -> str:
    """Where the magenta pixels are, from the third panel of compare's overlay."""
    if not diff_png.exists():
        return "pixels differ"
    im = Image.open(diff_png).convert("RGB")
    w3 = (im.width) // 3
    panel = im.crop((2 * w3, 0, 2 * w3 + w3 - 8, im.height))
    small = panel.copy()
    small.thumbnail((THUMB, THUMB))
    small.save(diff_jpg, quality=70)
    px = panel.resize((max(1, panel.width // 4), max(1, panel.height // 4)), Image.NEAREST).load()
    W, H = max(1, panel.width // 4), max(1, panel.height // 4)
    cells = [[0] * 3 for _ in range(3)]
    total = 0
    for y in range(H):
        for x in range(W):
            r, g, b = px[x, y]
            if r > 200 and g < 40 and b > 170:
                cells[min(2, y * 3 // H)][min(2, x * 3 // W)] += 1
                total += 1
    if not total:
        return "pixels differ (tiny areas)"
    where = [GRID[r][c] for r in range(3) for c in range(3) if cells[r][c] / total > 0.2]
    return "pixels differ mostly " + (", ".join(where) if where and len(where) < 5 else "across the page")


def page_rows(doc):
    name = doc["name"]
    s = slug(name)
    thumbs(actual_dir / name, img_dir / f"{s}.act")
    thumbs(expected_dir / name, img_dir / f"{s}.exp")
    ours, theirs = words(actual_dir / name), words(expected_dir / name)
    rows = []
    for page in range(1, doc["pages"] + 1):
        lay, txt = doc["scores"].get(page, (None, None))
        diff_png = diff_dir / f"{name.split('/')[-1].removesuffix('.pdf')}-p{page}.png"
        # compare writes a diff image exactly for failing pages; it settles a rounded "1.00".
        failed = lay is None or lay > 1 or (lay >= 1 and diff_png.exists())
        issues = []
        if lay is None:
            issues.append(doc["detail"] or doc["verdict"].lower())
        else:
            if failed:
                issues.append(layout_issue(diff_png, img_dir / f"{s}.diff-{page}.jpg"))
            if txt >= 1:
                issues.append("text: " + text_issue(ours.get(page, []), theirs.get(page, [])))
        rows.append((page, lay, txt, issues, failed))
    return doc, rows


def img(path: Path) -> str:
    rel = path.relative_to(report_dir)
    return f"![]({quote(str(rel))})" if path.exists() else "(none)"


def thumb(s: str, kind: str, page: int, pages: int) -> Path:
    # pdftoppm pads page numbers to the width of the page count.
    width = len(str(pages))
    return img_dir / f"{s}.{kind}-{page:0{width}d}.jpg"


with ThreadPoolExecutor(8) as pool:
    results = list(pool.map(page_rows, docs))

all_pages = [(d, r) for d, rows in results for r in rows]
scored = [r for _, r in all_pages if r[1] is not None]
passed = sum(1 for r in scored if not r[4])
avg = lambda v: sum(v) / len(v) if v else 0
out = [
    "# Rust converter acceptance report",
    "",
    f"Acceptance set: `IWorkToPdf/acceptance-set.txt`, {len(docs)} documents, {len(all_pages)} pages. "
    f"Documents passing: {sum(d['verdict'] == 'PASS' for d in docs)}. "
    f"Pages passing: {passed} of {len(scored)} scored ({len(all_pages) - len(scored)} not scored: page-count/size mismatch, errors or skipped huge pages).",
    "",
    f"Mean layout match {100 - avg([r[1] for r in scored]):.2f}%, mean text match {100 - avg([r[2] for r in scored]):.2f}%. "
    f"Pages with text match below 99%: {sum(1 for r in scored if r[2] >= 1)}.",
    "",
    "**Layout match** (pass criterion, the C# metric) = 100% - share of the whole page, text included, whose pixels differ "
    "(PDFium at 100 dpi, 2x2 box downsample, any RGB channel off by more than 32). A page passes above 99%. "
    "**Text match** (information) = 100% - share of characters without a counterpart of the same code point, origin within 1pt "
    "and colour within 32; text that we draw as pixels (rasterised pasted PDFs) counts as unmatched even when it looks right. "
    "Issues are derived per page: where the differing pixels are (from compare's diff mask) and a word-level diff of "
    "`pdftotext -bbox` output (shifted, missing, extra words). The diff column shows Apple's page faded, differing pixels in magenta.",
    "",
    "## Documents",
    "",
    "| Result | Document | Pages | Pages passing | Worst layout match | Worst text match | Note |",
    "|---|---|---|---|---|---|---|",
]
for doc, rows in sorted(results, key=lambda dr: (dr[0]["verdict"] == "PASS", dr[0]["name"])):
    sc = [r for r in rows if r[1] is not None]
    ok = sum(1 for r in sc if not r[4])
    wl = f"{100 - max(r[1] for r in sc):.2f}%" if sc else "n/a"
    wt = f"{100 - max(r[2] for r in sc):.2f}%" if sc else "n/a"
    anchor = slug(doc["name"]).lower().replace(".", "")
    out.append(f"| {doc['verdict']} | [{doc['name']}](#{anchor}) | {doc['pages']} | {ok} | {wl} | {wt} | {doc['detail'].replace('|', '/')} |")
out += ["", "## Pages", ""]
for doc, rows in results:
    s = slug(doc["name"])
    out += [f"### {s}", "", f"`{doc['name']}` — {doc['verdict']}" + (f" — {doc['detail']}" if doc['detail'] else ""), "",
            "| Page | Apple (expected) | Rust (actual) | Layout diff | Layout match | Text match | Remaining issues |",
            "|---|---|---|---|---|---|---|"]
    for page, lay, txt, issues, failed in rows:
        e = img(thumb(s, "exp", page, doc["pages"]))
        a = img(thumb(s, "act", page, sum(1 for _ in img_dir.glob(f"{s}.act-*.jpg")) or doc["pages"]))
        d = img(img_dir / f"{s}.diff-{page}.jpg") if lay is not None and failed else ""
        lm = f"{100 - lay:.2f}%" if lay is not None else "n/a"
        tm = f"{100 - txt:.2f}%" if txt is not None else "n/a"
        iss = "; ".join(issues).replace("|", "/") if issues else "none"
        out.append(f"| {page} | {e} | {a} | {d} | {lm} | {tm} | {iss} |")
    out.append("")
(report_dir / "REPORT.md").write_text("\n".join(out))
print(f"{len(docs)} docs, {len(all_pages)} pages, {passed} passing pages")
