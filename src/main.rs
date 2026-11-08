//! iwork2pdf: convert Apple iWork documents (.numbers, .key, .pages) to PDF, offline.

use anyhow::{bail, Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use iwork2pdf::compare::{self, Cache, FileScore, Options};
use iwork2pdf::{fonts, render};
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const USAGE: &str = "\
iwork2pdf - convert Apple iWork documents (.numbers, .key, .pages) to PDF, offline.

USAGE
  iwork2pdf convert <document> [-o <out.pdf>] [--fonts <dir>]
  iwork2pdf batch <src-dir> <out-dir> [--set <list.txt>] [--fonts <dir>]
  iwork2pdf compare <actual.pdf|dir> <expected.pdf|dir> [--set <list.txt>] [--diff-dir <dir>]
                    [--cache <dir>] [--procs <n>]

DEBUG
  iwork2pdf render <pdf> <out-dir> [--dpi 72]     rasterise every page to PNG
  iwork2pdf fonts [<dir>]                         PostScript names used as exact fonts

--fonts <dir> (or IWORK2PDF_FONTS) points at licensed font files (.ttf/.otf/.ttc); a document font
found there by PostScript name is used directly instead of a substitute. On macOS the installed
Apple fonts are used the same way.

'batch' converts every document under <src-dir> (or only those whose <rel>.pdf is listed in --set)
to <out-dir>/<rel>.pdf. 'compare' renders each whole page in colour and passes it when less than
1% of its area differs; it also reports, for information, the share of characters (code, position,
colour) without a counterpart. PDF rasterising needs libpdfium (IWORK2PDF_PDFIUM or next to the binary).
IWORK2PDF_PAGES=1 makes 'compare' list every page's visual%/text% after each file's line.
IWORK2PDF_VERBOSE=1 (or RUST_LOG=debug) logs layout decisions.";

/// Command-line arguments: positional values and `--name value` options (`-o` is `--out`).
#[derive(Debug, Default)]
struct Arguments {
    positional: Vec<String>,
    options: HashMap<String, String>,
}

impl Arguments {
    fn parse(raw: &[String]) -> Result<Arguments> {
        let mut parsed = Arguments::default();
        let mut values = raw.iter();
        while let Some(value) = values.next() {
            let name = match value.as_str() {
                "-o" => Some("out"),
                option => option.strip_prefix("--"),
            };
            match name {
                Some(name) => {
                    let Some(option_value) = values.next() else {
                        bail!("{value} needs a value")
                    };
                    parsed.options.insert(name.to_owned(), option_value.clone());
                }
                None => parsed.positional.push(value.clone()),
            }
        }
        Ok(parsed)
    }

    fn option(&self, name: &str) -> Option<&str> {
        self.options.get(name).map(String::as_str)
    }
}

fn main() {
    let verbose = std::env::var("IWORK2PDF_VERBOSE").as_deref() == Ok("1");
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(if verbose {
        "debug"
    } else {
        "warn"
    }))
    .init();
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let code = match run(&raw) {
        Ok(code) => code,
        Err(error) => {
            log::error!("{error:#}");
            eprintln!("error: {error:#}\n\n{USAGE}");
            2
        }
    };
    std::process::exit(code);
}

fn run(raw: &[String]) -> Result<i32> {
    let Some(command) = raw.first() else {
        println!("{USAGE}");
        return Ok(0);
    };
    let arguments = Arguments::parse(&raw[1..])?;
    if let Some(dir) = arguments.option("fonts") {
        fonts::set_user_font_dir(dir);
    }
    match command.as_str() {
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            Ok(0)
        }
        "convert" => convert_one(&arguments),
        "batch" => batch(&arguments),
        "compare" => compare_command(&arguments, raw),
        "render" => render_command(&arguments),
        "fonts" => {
            if let Some(dir) = arguments.positional.first() {
                fonts::set_user_font_dir(dir);
            }
            fonts::user_font_names()
                .iter()
                .for_each(|name| println!("{name}"));
            Ok(0)
        }
        other => bail!("unknown command '{other}'"),
    }
}

fn is_document(path: &Path) -> bool {
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase());
    matches!(extension.as_deref(), Some("pages" | "numbers" | "key"))
}

/// The relative names listed in a --set file ('\' normalised to '/').
fn load_set(arguments: &Arguments) -> Result<Option<HashSet<String>>> {
    let Some(path) = arguments.option("set") else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    Ok(Some(
        text.lines()
            .map(|line| line.trim().replace('\\', "/"))
            .filter(|line| !line.is_empty())
            .collect(),
    ))
}

fn convert_one(arguments: &Arguments) -> Result<i32> {
    let [document] = arguments.positional.as_slice() else {
        bail!("convert takes exactly one document")
    };
    let document = PathBuf::from(document.trim_end_matches(['/', '\\']));
    if !document.exists() {
        bail!("not found: {}", document.display());
    }
    let output = arguments.option("out").map_or_else(
        || PathBuf::from(format!("{}.pdf", document.display())),
        PathBuf::from,
    );
    match render::convert(&document, &output) {
        Ok(pages) => {
            eprintln!("wrote {} ({pages} page(s))", output.display());
            Ok(0)
        }
        Err(error) => {
            log::error!("{}: {error}", document.display());
            Ok(1)
        }
    }
}

/// Documents under a directory: files or package directories with an iWork extension (packages are
/// not descended into), in sorted order.
fn find_documents(dir: &Path, found: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("listing {}", dir.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    entries.sort();
    for path in entries {
        if is_document(&path) {
            found.push(path);
        } else if path.is_dir() {
            find_documents(&path, found)?;
        }
    }
    Ok(())
}

fn progress(count: usize, message: &'static str) -> ProgressBar {
    let bar = ProgressBar::new(count as u64);
    let style = ProgressStyle::with_template("{msg} [{bar:40}] {pos}/{len} {elapsed}<{eta}")
        .unwrap_or_else(|_| ProgressStyle::default_bar());
    bar.set_style(style.progress_chars("=> "));
    bar.set_message(message);
    bar
}

fn batch(arguments: &Arguments) -> Result<i32> {
    let [source, output] = arguments.positional.as_slice() else {
        bail!("batch takes <src-dir> <out-dir>")
    };
    let (source, output) = (PathBuf::from(source), PathBuf::from(output));
    let set = load_set(arguments)?;
    let mut documents = Vec::new();
    find_documents(&source, &mut documents)?;
    let relative = |path: &Path| {
        path.strip_prefix(&source)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    };
    let selected: Vec<String> = documents
        .iter()
        .map(|path| relative(path))
        .filter(|name| {
            set.as_ref()
                .is_none_or(|set| set.contains(&format!("{name}.pdf")))
        })
        .collect();
    eprintln!("{} documents, {} selected", documents.len(), selected.len());
    let bar = progress(selected.len(), "converting");
    // Each document converts in its own child process (`convert -o`): PDFium is locked per
    // process, so in-process threads would queue on it, and a crash (abort, segfault) in one
    // document only fails that document. Rayon keeps one child running per core.
    let executable = std::env::current_exe()?;
    let failures: Vec<&String> = selected
        .par_iter()
        .filter(|name| {
            let target = output.join(format!("{name}.pdf"));
            let mut command = Command::new(&executable);
            command
                .arg("convert")
                .arg(source.join(name))
                .arg("-o")
                .arg(&target)
                .stdout(Stdio::null());
            if let Some(dir) = arguments.option("fonts") {
                command.args(["--fonts", dir]);
            }
            let status = std::fs::create_dir_all(target.parent().unwrap_or(&output))
                .and_then(|()| command.output());
            bar.inc(1);
            // `output()` captures stderr so the child's "wrote ..." line does not break the bar.
            let error = match status {
                Ok(finished) if finished.status.success() => return false,
                Ok(finished) => match finished.status.code() {
                    Some(_) => String::from_utf8_lossy(&finished.stderr).trim().to_owned(),
                    None => format!("crashed ({})", finished.status),
                },
                Err(error) => error.to_string(),
            };
            bar.suspend(|| log::error!("{name}: {error}"));
            true
        })
        .collect();
    bar.finish_and_clear();
    eprintln!(
        "done: ok={} failed={}",
        selected.len() - failures.len(),
        failures.len()
    );
    Ok(i32::from(!failures.is_empty()))
}

/// One comparison: the file's name, our PDF and Apple's.
type Pair = (String, PathBuf, PathBuf);

fn compare_pairs(arguments: &Arguments) -> Result<Vec<Pair>> {
    let [actual, expected] = arguments.positional.as_slice() else {
        bail!("compare takes <actual> <expected>")
    };
    let (actual, expected) = (PathBuf::from(actual), PathBuf::from(expected));
    if !expected.is_dir() {
        let name = expected
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        return Ok(vec![(name, actual, expected)]);
    }
    let set = load_set(arguments)?;
    let mut pdfs = Vec::new();
    collect_pdfs(&expected, &mut pdfs)?;
    pdfs.sort();
    Ok(pdfs
        .into_iter()
        .filter_map(|path| {
            let name = path
                .strip_prefix(&expected)
                .ok()?
                .to_string_lossy()
                .replace('\\', "/");
            set.as_ref()
                .is_none_or(|set| set.contains(&name))
                .then(|| (name.clone(), actual.join(&name), path))
        })
        .collect())
}

fn collect_pdfs(dir: &Path, found: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("listing {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            collect_pdfs(&path, found)?;
        } else if path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
        {
            found.push(path);
        }
    }
    Ok(())
}

/// The result line of one file and whether it passed.
fn score_line(name: &str, actual: &Path, expected: &Path, options: &Options) -> (String, bool) {
    if !actual.exists() {
        return (
            format!(
                "{:<7} {:>5} {:>7.2} {:>7.2} {:>3}  {name}",
                "MISSING", 0, 0.0, 0.0, 0
            ),
            false,
        );
    }
    let result: std::result::Result<FileScore, _> = compare::compare(actual, expected, options);
    let (verdict, pages, visual, text, worst_page, detail) = match result {
        Ok(score) => {
            let worst = score.worst();
            let mut detail = score
                .error
                .clone()
                .or_else(|| score.pages.iter().find_map(|page| page.error.clone()))
                .unwrap_or_default();
            // IWORK2PDF_PAGES=1 lists every page's "page visual%/text%" after the detail.
            if std::env::var("IWORK2PDF_PAGES").as_deref() == Ok("1") && !score.pages.is_empty() {
                let pages: Vec<String> = score
                    .pages
                    .iter()
                    .map(|page| {
                        format!(
                            "{} {:.2}/{:.2}",
                            page.page,
                            page.visual * 100.0,
                            page.text * 100.0
                        )
                    })
                    .collect();
                let separator = if detail.is_empty() { "" } else { "; " };
                detail = format!("{detail}{separator}pages {}", pages.join(", "));
            }
            let verdict = if detail.starts_with("skipped") {
                "SKIP"
            } else if score.passed() {
                "PASS"
            } else {
                "FAIL"
            };
            let visual = score
                .pages
                .iter()
                .map(|page| page.visual)
                .fold(if score.error.is_some() { 1.0 } else { 0.0 }, f64::max);
            let text = score
                .pages
                .iter()
                .map(|page| page.text)
                .fold(if score.error.is_some() { 1.0 } else { 0.0 }, f64::max);
            (
                verdict,
                score.expected_pages,
                visual,
                text,
                worst.map_or(0, |page| page.page),
                detail,
            )
        }
        Err(error) => ("ERROR", 0, 0.0, 0.0, 0, error.to_string()),
    };
    let detail = if detail.is_empty() {
        String::new()
    } else {
        format!("  -- {detail}")
    };
    let line = format!(
        "{verdict:<7} {pages:>5} {:>7.2} {:>7.2} {worst_page:>3}  {name}{detail}",
        visual * 100.0,
        text * 100.0
    );
    (line, verdict == "PASS")
}

fn compare_command(arguments: &Arguments, raw: &[String]) -> Result<i32> {
    let pairs = compare_pairs(arguments)?;
    let options = Options {
        diff_dir: arguments.option("diff-dir").map(PathBuf::from),
        cache: arguments
            .option("cache")
            .or(std::env::var("IWORK2PDF_COMPARE_CACHE").ok().as_deref())
            .map(|dir| Cache::new(Path::new(dir))),
        full: std::env::var("IWORK2PDF_FULL").as_deref() == Ok("1"),
    };
    // A worker process: compare its share of the files and print "<index>\t<P|F>\t<line>".
    if let Some(shard) = arguments.option("shard") {
        let (worker, workers) = shard.split_once('/').context("--shard takes k/n")?;
        let (worker, workers): (usize, usize) = (worker.parse()?, workers.parse()?);
        let mut stdout = std::io::stdout().lock();
        for (index, (name, actual, expected)) in pairs
            .iter()
            .enumerate()
            .filter(|(index, _)| index % workers == worker)
        {
            let (line, passed) = score_line(name, actual, expected, &options);
            writeln!(
                stdout,
                "{index}\t{}\t{line}",
                if passed { 'P' } else { 'F' }
            )?;
            stdout.flush()?;
        }
        return Ok(0);
    }
    // PDFium serialises every call within a process, so the files are split over worker processes
    // (this command with --shard k/n); their lines are printed back in file order.
    let default_workers =
        std::thread::available_parallelism().map_or(1, |cores| (cores.get() / 2).max(1));
    let workers: usize = arguments
        .option("procs")
        .map(str::parse)
        .transpose()?
        .unwrap_or(default_workers)
        .clamp(1, pairs.len().max(1));
    let executable = std::env::current_exe()?;
    let mut children = (0..workers)
        .map(|worker| {
            Command::new(&executable)
                .args(raw)
                .args(["--shard", &format!("{worker}/{workers}")])
                .stdout(Stdio::piped())
                .spawn()
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    let (sender, receiver) = std::sync::mpsc::channel::<(usize, bool, String)>();
    for child in &mut children {
        let Some(stdout) = child.stdout.take() else {
            continue;
        };
        let sender = sender.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout)
                .lines()
                .map_while(std::io::Result::ok)
            {
                let mut fields = line.splitn(3, '\t');
                if let (Some(Ok(index)), Some(verdict), Some(text)) =
                    (fields.next().map(str::parse), fields.next(), fields.next())
                {
                    let _ = sender.send((index, verdict == "P", text.to_owned()));
                }
            }
        });
    }
    drop(sender);
    println!(
        "{:<7} {:>5} {:>7} {:>7} {:>3}  file",
        "result", "pages", "visual%", "text%", "pg"
    );
    let bar = progress(pairs.len(), "comparing");
    let mut pending: BTreeMap<usize, (bool, String)> = BTreeMap::new();
    let mut next = 0;
    let mut passed = 0;
    let mut skipped = 0;
    for (index, ok, line) in receiver {
        bar.inc(1);
        pending.insert(index, (ok, line));
        while let Some((ok, line)) = pending.remove(&next) {
            bar.suspend(|| println!("{line}"));
            passed += usize::from(ok);
            skipped += usize::from(line.starts_with("SKIP"));
            next += 1;
        }
    }
    for child in &mut children {
        child.wait()?;
    }
    bar.finish_and_clear();
    for (index, (name, _, _)) in pairs.iter().enumerate().skip(next) {
        if !pending.contains_key(&index) {
            println!(
                "{:<7} {:>5} {:>7.2} {:>7.2} {:>3}  {name}  -- compare worker exited early",
                "ERROR", 0, 0.0, 0.0, 0
            );
        }
    }
    let compared = pairs.len() - skipped;
    let failed = compared - passed;
    println!(
        "\n{passed} passed, {failed} failed of {compared}{} (a page passes when less than 1% of its area differs; {} dpi colour, 2x2 box downsample, channel tolerance {}; text% is information only: characters within {}pt)",
        if skipped > 0 { format!(" ({skipped} skipped; IWORK2PDF_FULL=1 compares huge pages)") } else { String::new() },
        compare::DPI,
        compare::CHANNEL_TOLERANCE,
        compare::POSITION_TOLERANCE,
    );
    Ok(i32::from(failed > 0))
}

fn render_command(arguments: &Arguments) -> Result<i32> {
    let [pdf, output] = arguments.positional.as_slice() else {
        bail!("render takes <pdf> <out-dir>")
    };
    let dpi: f32 = arguments.option("dpi").unwrap_or("72").parse()?;
    let pdfium = iwork2pdf::pdfium::get().context("libpdfium not found (set IWORK2PDF_PDFIUM)")?;
    let document = pdfium.load_pdf_from_file(pdf, None)?;
    std::fs::create_dir_all(output)?;
    let stem = Path::new(pdf)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    for (index, page) in document.pages().iter().enumerate() {
        let (width, height, rgba) =
            iwork2pdf::pdfium::render(&page, dpi, Some(pdfium_render::prelude::PdfColor::WHITE))
                .map_err(anyhow::Error::msg)?;
        let image =
            iwork2pdf::drawables::raster(width, height, &rgba, skia_safe::AlphaType::Unpremul)
                .context("rasterising")?;
        let png = image
            .encode(None, skia_safe::EncodedImageFormat::PNG, None)
            .context("PNG encoding")?;
        std::fs::write(
            Path::new(output).join(format!("{stem}-p{}.png", index + 1)),
            png.as_bytes(),
        )?;
        println!("page {}: {width}x{height}", index + 1);
    }
    Ok(0)
}
