#!/bin/sh
# Full acceptance run of the Rust converter against Apple's PDFs, page by page.
# Every run first deletes the previous outputs in the project folder (out-rust, out-rust-diff,
# acceptance-report) so they never pile up; Apple's rendered pages stay cached in compare-cache.
#   ./acceptance.sh  ->  ../compare-rust.txt, ../out-rust-diff/*.png (failing pages),
#                        ../acceptance-report/REPORT.md (side-by-side page table)
set -e
cd "$(dirname "$0")/.."
B=iworktopdf-rust/target/release/iwork2pdf
CORPUS=iwork-corpus-part01-of-07
SET=IWorkToPdf/acceptance-set.txt
rm -rf out-rust out-rust-diff acceptance-report compare-rust.txt
(cd iworktopdf-rust && cargo build --release)
$B batch "$CORPUS/iwork-corpus" out-rust --set "$SET" || true
IWORK2PDF_PAGES=1 IWORK2PDF_FULL=1 $B compare out-rust "$CORPUS/iwork-corpus-expected-pdf" \
    --set "$SET" --diff-dir out-rust-diff --cache compare-cache > compare-rust.txt || true
tail -1 compare-rust.txt
python3 iworktopdf-rust/acceptance-report.py compare-rust.txt out-rust \
    "$CORPUS/iwork-corpus-expected-pdf" out-rust-diff acceptance-report
