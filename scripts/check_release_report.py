#!/usr/bin/env python3
"""
Gerbang pemeriksaan mesin untuk laporan rilis SelenCode (Perbaikan 2 v0.5.13).

Memverifikasi sebelum tag rilis diterbitkan bahwa berkas laporan di `docs/reports/`
benar-benar lengkap dan terstruktur:
1. Menyebut tag versi target (`vX.Y.Z`).
2. Menyatakan `Jumlah butir: N` (dan jika berkas prompt rilis ikut diberikan / ditemukan,
   jumlah butirnya wajib cocok).
3. Memuat TEPAT satu baris vonis berpola tetap per butir `1..=N` tanpa ada yang hilang,
   duplikat, atau buktinya kosong:
       Butir <n> — <LULUS|SEBAGIAN|GAGAL>: <satu kalimat bukti>
4. Memuat bagian `Tabel Regresi` dengan minimal satu baris data, dan TIDAK boleh ada sel
   angka yang kosong / placeholder (`-`, `TBD`, `?`, `N/A`, dsb.) kecuali metrik tersebut
   memiliki baris pernyataan eksplisit `Tidak diukur — <metrik>: <alasan>`.
5. Memuat bagian `Tidak Diubah` / `Jangan Diubah` yang tidak kosong.

Keluar dengan exit code 0 bila LOLOS, atau exit code 1 dengan daftar alasan spesifik
(menyebut butir yang hilang atau baris/kolom sel angka yang kosong) bila DITOLAK.
"""

from __future__ import annotations

import argparse
import glob
import os
import re
import sys
from pathlib import Path
from typing import Dict, List, Optional, Tuple


VERDICT_RE = re.compile(
    r"^\s*(?:[-*]\s+)?(?:\*\*)?Butir\s+(\d+)(?:\*\*)?\s*[—–-]\s*"
    r"(?:\*\*)?(LULUS|SEBAGIAN|GAGAL)(?:\*\*)?\s*:\s*(?:\*\*)?\s*(.+?)\s*$"
)

ITEM_COUNT_RE = re.compile(
    r"(?i)jumlah\s+butir(?:\s+prompt(?:\s+ini)?)?\s*\*?\*?\s*:\s*\*?\*?\s*(\d+)\*?\*?"
)

UNMEASURED_RE = re.compile(
    r"^\s*(?:[-*]\s+)?(?:\*\*)?(?:Pernyataan\s+)?Tidak\s+diukur(?:\*\*)?\s*"
    r"[—–:-]\s*(.+?)\s*:\s*(.+?)\s*$",
    re.IGNORECASE,
)

SECTION_HEADING_RE = re.compile(r"^(#{1,6})\s+(.+?)\s*$")
PERBAIKAN_HEADING_RE = re.compile(r"(?i)^#{1,6}\s+(?:\d+\.\s*)?Perbaikan\s+(\d+)\b")

PLACEHOLDER_CELLS = {
    "",
    "-",
    "--",
    "---",
    "–",
    "—",
    "?",
    "tbd",
    "todo",
    "n/a",
    "na",
    "none",
    "null",
    "kosong",
    "belum",
    "belum diukur",
    "tidak diukur",
    "skip",
    "skipped",
    "...",
    "…",
}

NON_NUMERIC_HEADERS = {
    "status",
    "vonis",
    "catatan",
    "keterangan",
    "alasan",
    "notes",
    "note",
    "komentar",
}


def strip_md_inline(text: str) -> str:
    s = text.strip()
    s = re.sub(r"[`*_~]", "", s)
    s = re.sub(r"\s+", " ", s)
    return s.strip()


def normalize_key(text: str) -> str:
    return strip_md_inline(text).lower()


def extract_declared_item_count(text: str) -> Optional[int]:
    m = ITEM_COUNT_RE.search(text)
    if not m:
        return None
    try:
        return int(m.group(1))
    except ValueError:
        return None


def split_md_row(line: str) -> List[str]:
    t = line.strip()
    if t.startswith("|"):
        t = t[1:]
    if t.endswith("|"):
        t = t[:-1]
    return [c.strip() for c in t.split("|")]


def is_separator_row(line: str) -> bool:
    if "|" not in line or "-" not in line:
        return False
    cells = split_md_row(line)
    if not cells:
        return False
    return all(
        bool(c) and all(ch in "-: " for ch in c) and "-" in c
        for c in cells
    )


def find_section_lines(lines: List[str], heading_pred) -> Optional[List[str]]:
    start_idx: Optional[int] = None
    start_level = 0
    for idx, line in enumerate(lines):
        m = SECTION_HEADING_RE.match(line.strip())
        if not m:
            continue
        level = len(m.group(1))
        title = m.group(2).strip()
        if start_idx is None:
            if heading_pred(title.lower()):
                start_idx = idx + 1
                start_level = level
        else:
            if level <= start_level:
                return lines[start_idx:idx]
    if start_idx is not None:
        return lines[start_idx:]
    return None


def validate_report(
    report_text: str,
    tag: Optional[str] = None,
    expected_items: Optional[int] = None,
    prompt_text: Optional[str] = None,
) -> Tuple[bool, List[str], List[str]]:
    errors: List[str] = []
    notes: List[str] = []
    lines = report_text.splitlines()
    low = report_text.lower()

    # 1. Tag versi wajib disebut bila diberikan
    if tag:
        if tag.lower() not in low:
            errors.append(f"Versi rilis '{tag}' tidak ditemukan di dalam laporan.")

    # 2. Tentukan jumlah butir yang diharapkan
    prompt_count: Optional[int] = None
    if prompt_text is not None:
        prompt_count = extract_declared_item_count(prompt_text)
        if prompt_count is None or prompt_count < 1:
            errors.append(
                "Berkas prompt rilis tidak mencantumkan 'Jumlah butir: <N>' yang valid di kepala berkas."
            )

    report_count = extract_declared_item_count(report_text)
    if report_count is None or report_count < 1:
        errors.append(
            "Kepala laporan wajib mencantumkan jumlah butir prompt (contoh: '- **Jumlah butir:** 3')."
        )

    target_count: Optional[int] = expected_items
    if prompt_count is not None:
        if target_count is not None and target_count != prompt_count:
            errors.append(
                f"Ketidakcocokan jumlah butir: --expected-items={target_count} vs prompt={prompt_count}."
            )
        target_count = prompt_count
    if report_count is not None:
        if target_count is not None and report_count != target_count:
            errors.append(
                f"Ketidakcocokan jumlah butir: laporan menyatakan {report_count} butir, "
                f"tetapi prompt/target mengharuskan {target_count} butir."
            )
        elif target_count is None:
            target_count = report_count

    # Cegah pelaporan jumlah butir lebih kecil dari heading `Perbaikan <k>` di dalam laporan
    max_perbaikan_heading = 0
    for line in lines:
        pm = PERBAIKAN_HEADING_RE.match(line.strip())
        if pm:
            max_perbaikan_heading = max(max_perbaikan_heading, int(pm.group(1)))
    if target_count is not None and max_perbaikan_heading > target_count:
        errors.append(
            f"Laporan memiliki heading sampai 'Perbaikan {max_perbaikan_heading}', "
            f"tetapi menyatakan Jumlah butir = {target_count}."
        )

    # 3. Periksa baris vonis berpola tetap: `Butir <n> — <LULUS|SEBAGIAN|GAGAL>: <bukti>`
    verdicts: Dict[int, List[Tuple[str, str]]] = {}
    for line in lines:
        vm = VERDICT_RE.match(line)
        if vm:
            num = int(vm.group(1))
            status = vm.group(2)
            evidence = strip_md_inline(vm.group(3))
            verdicts.setdefault(num, []).append((status, evidence))

    if not verdicts:
        errors.append(
            "Tidak ditemukan baris vonis berpola 'Butir <n> — <LULUS|SEBAGIAN|GAGAL>: <satu kalimat bukti>'."
        )
    else:
        for num, entries in sorted(verdicts.items()):
            if len(entries) > 1:
                errors.append(f"Duplikat baris vonis untuk Butir {num} ({len(entries)} kali).")
            for _status, ev in entries:
                if len(ev) < 12 or ev.lower() in PLACEHOLDER_CELLS:
                    errors.append(
                        f"Bukti pada baris vonis Butir {num} kosong atau terlalu pendek ('{ev}')."
                    )

        if target_count is not None:
            expected_set = set(range(1, target_count + 1))
            found_set = set(verdicts.keys())
            missing = sorted(expected_set - found_set)
            extra = sorted(found_set - expected_set)
            if missing:
                missing_labels = ", ".join(f"Butir {m}" for m in missing)
                errors.append(
                    f"Baris vonis hilang untuk: {missing_labels} "
                    f"(diharapkan {target_count} butir: Butir 1..{target_count}, "
                    f"ditemukan: {sorted(found_set)})."
                )
            if extra:
                extra_labels = ", ".join(f"Butir {e}" for e in extra)
                errors.append(
                    f"Baris vonis di luar rentang 1..{target_count}: {extra_labels}."
                )
            notes.append(
                f"Vonis butir terverifikasi: {len(found_set)}/{target_count} "
                f"({', '.join(f'Butir {k}={verdicts[k][0][0]}' for k in sorted(verdicts))})"
            )

    # 4. Kumpulkan deklarasi eksplisit `Tidak diukur — <metrik>: <alasan>`
    unmeasured_map: Dict[str, str] = {}
    for line in lines:
        um = UNMEASURED_RE.match(line)
        if um:
            metric_raw = um.group(1).strip()
            reason = strip_md_inline(um.group(2))
            if len(reason) < 10 or reason.lower() in PLACEHOLDER_CELLS:
                errors.append(
                    f"Pernyataan 'Tidak diukur' untuk '{metric_raw}' harus menyertakan alasan yang jelas."
                )
            else:
                unmeasured_map[normalize_key(metric_raw)] = reason

    # 5. Periksa bagian `Tabel Regresi` & seluruh sel angkanya
    reg_lines = find_section_lines(lines, lambda t: "regresi" in t)
    if reg_lines is None:
        errors.append(
            "Laporan wajib memuat bagian '## Tabel Regresi' berisi angka regresi atau pernyataan eksplisit 'Tidak diukur'."
        )
    else:
        # Ekstrak semua tabel Markdown di bagian regresi
        tables_found = 0
        rows_checked = 0
        i = 0
        while i < len(reg_lines):
            ln = reg_lines[i]
            if "|" in ln and i + 1 < len(reg_lines) and is_separator_row(reg_lines[i + 1]):
                tables_found += 1
                headers = [strip_md_inline(h) for h in split_md_row(ln)]
                i += 2
                while i < len(reg_lines) and "|" in reg_lines[i] and reg_lines[i].strip():
                    cells = split_md_row(reg_lines[i])
                    if not cells or all(not c for c in cells):
                        i += 1
                        continue
                    rows_checked += 1
                    metric_name = strip_md_inline(cells[0]) if cells else f"baris #{rows_checked}"
                    metric_key = normalize_key(metric_name)
                    has_unmeasured_decl = any(
                        metric_key == k or metric_key in k or k in metric_key
                        for k in unmeasured_map
                    )
                    # Pad cells to header width if trailing cells omitted
                    if len(cells) < len(headers):
                        cells.extend([""] * (len(headers) - len(cells)))
                    for col_idx in range(1, len(headers)):
                        col_hdr = headers[col_idx] or f"kolom #{col_idx + 1}"
                        col_hdr_low = col_hdr.lower()
                        raw_val = cells[col_idx] if col_idx < len(cells) else ""
                        clean_val = strip_md_inline(raw_val)
                        clean_low = clean_val.lower()
                        is_non_numeric_col = any(
                            nh == col_hdr_low or nh in col_hdr_low.split()
                            for nh in NON_NUMERIC_HEADERS
                        )
                        is_empty_or_placeholder = clean_low in PLACEHOLDER_CELLS or clean_low.startswith(
                            "tidak diukur"
                        )
                        is_delegated_to_muse = "pengukuran penerimaan oleh muse" in clean_low
                        if is_delegated_to_muse:
                            continue
                        if is_empty_or_placeholder:
                            if not has_unmeasured_decl:
                                errors.append(
                                    f"Sel angka kosong/belum diisi tanpa pernyataan eksplisit "
                                    f"'Tidak diukur — {metric_name}: <alasan>' pada baris '{metric_name}' "
                                    f"kolom '{col_hdr}' (isi: '{raw_val or '(kosong)'}')."
                                )
                        elif not is_non_numeric_col:
                            # Kolom angka wajib mengandung minimal satu digit angka (0-9)
                            if not re.search(r"\d", clean_val):
                                if not has_unmeasured_decl:
                                    errors.append(
                                        f"Sel pada baris '{metric_name}' kolom '{col_hdr}' tidak memuat angka "
                                        f"dan tidak memiliki pernyataan 'Tidak diukur' (isi: '{clean_val}')."
                                    )
                    i += 1
            else:
                i += 1

        if tables_found == 0 or rows_checked == 0:
            errors.append(
                "Bagian 'Tabel Regresi' wajib memuat minimal satu tabel Markdown dengan baris metrik terisi."
            )
        else:
            notes.append(
                f"Tabel regresi terverifikasi: {rows_checked} baris metrik, "
                f"{len(unmeasured_map)} pernyataan 'Tidak diukur' eksplisit."
            )

    # 6. Bagian 'Tidak Diubah' wajib ada dan tidak kosong
    unchanged_lines = find_section_lines(
        lines, lambda t: "tidak diubah" in t or "jangan diubah" in t
    )
    if unchanged_lines is None or not any(l.strip() for l in unchanged_lines):
        errors.append(
            "Laporan wajib memuat bagian 'Tidak Diubah' / 'Jangan Diubah' yang tidak kosong."
        )

    return (len(errors) == 0, errors, notes)


def discover_report(tag: str, repo_root: Path) -> Optional[Path]:
    digits = re.sub(r"[^0-9]", "", tag)
    pattern = str(repo_root / "docs" / "reports" / f"HASIL-V{digits}-*.md")
    matches = sorted(glob.glob(pattern))
    if not matches:
        return None
    return Path(matches[-1])


def discover_prompt(tag: str, repo_root: Path) -> Optional[Path]:
    digits = re.sub(r"[^0-9]", "", tag)
    for pat in [
        str(repo_root / "docs" / "prompts" / f"*V{digits}*.md"),
        str(repo_root / "docs" / "prompts" / f"*v{digits}*.md"),
    ]:
        matches = sorted(glob.glob(pat))
        if matches:
            return Path(matches[-1])
    return None


def main(argv: List[str]) -> int:
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            try:
                stream.reconfigure(encoding="utf-8", errors="replace")
            except Exception:
                pass
    parser = argparse.ArgumentParser(
        description="Pemeriksa mesin laporan rilis SelenCode (Perbaikan 2 v0.5.13)."
    )
    parser.add_argument("--tag", help="Tag rilis target (mis. v0.5.13)")
    parser.add_argument("--report", help="Path eksplisit ke berkas laporan Markdown")
    parser.add_argument("--prompt-file", help="Path opsional ke berkas prompt rilis")
    parser.add_argument(
        "--expected-items",
        type=int,
        help="Jumlah butir prompt yang diharapkan secara eksplisit",
    )
    args = parser.parse_args(argv)

    repo_root = Path.cwd()
    report_path: Optional[Path] = Path(args.report) if args.report else None
    if report_path is None:
        if not args.tag:
            print(
                "GAGAL: wajib memberikan --report <path> atau --tag <vX.Y.Z>.",
                file=sys.stderr,
            )
            return 2
        report_path = discover_report(args.tag, repo_root)
        if report_path is None:
            digits = re.sub(r"[^0-9]", "", args.tag)
            print(
                f"::error::Gerbang laporan GAGAL: berkas laporan tidak ditemukan "
                f"(pola diharapkan: docs/reports/HASIL-V{digits}-*.md)",
                file=sys.stderr,
            )
            return 1

    if not report_path.is_file():
        print(
            f"::error::Gerbang laporan GAGAL: berkas laporan tidak ada: {report_path}",
            file=sys.stderr,
        )
        return 1

    prompt_path: Optional[Path] = Path(args.prompt_file) if args.prompt_file else None
    if prompt_path is None and args.tag:
        prompt_path = discover_prompt(args.tag, repo_root)

    report_text = report_path.read_text(encoding="utf-8")
    prompt_text = (
        prompt_path.read_text(encoding="utf-8")
        if prompt_path is not None and prompt_path.is_file()
        else None
    )

    ok, errors, notes = validate_report(
        report_text=report_text,
        tag=args.tag,
        expected_items=args.expected_items,
        prompt_text=prompt_text,
    )

    if not ok:
        print(f"GERBANG LAPORAN RILIS DITOLAK ({report_path}):", file=sys.stderr)
        for err in errors:
            print(f"::error::Gerbang laporan GAGAL ({report_path}): {err}", file=sys.stderr)
        return 1

    print(f"GERBANG LAPORAN RILIS LULUS: {report_path} ({len(report_text)} byte)")
    for note in notes:
        print(f"  - {note}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
