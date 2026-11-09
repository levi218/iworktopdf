const root = document.documentElement;
const picker = document.getElementById("picker");
const dropZone = document.getElementById("drop-zone");
const status = document.getElementById("status");
const result = document.getElementById("result");
const preview = document.getElementById("preview");
const download = document.getElementById("download");
const localFontsButton = document.getElementById("local-fonts");
const fontFiles = document.getElementById("font-files");
const fontStatus = document.getElementById("font-status");
const worker = new Worker("worker.js");
let pdfUrl = null;
let busy = false;
// Installed fonts (FontData), once the user allowed access.
let localFonts = null;
// Font file names already sent to the worker.
const sentFonts = new Set();

// Theme: follows the system until toggled; the choice is remembered per browser.
const themeToggle = document.getElementById("theme-toggle");
const systemDark = matchMedia("(prefers-color-scheme: dark)");
const effectiveTheme = () => root.dataset.theme || (systemDark.matches ? "dark" : "light");
try {
  const saved = localStorage.getItem("theme");
  if (saved) root.dataset.theme = saved;
} catch {}
const labelToggle = () => {
  const next = effectiveTheme() === "dark" ? "light" : "dark";
  themeToggle.setAttribute("aria-label", `Switch to ${next} theme`);
};
themeToggle.addEventListener("click", () => {
  root.dataset.theme = effectiveTheme() === "dark" ? "light" : "dark";
  try {
    localStorage.setItem("theme", root.dataset.theme);
  } catch {}
  labelToggle();
});
systemDark.addEventListener("change", labelToggle);
labelToggle();

const showStatus = (text, isError = false) => {
  status.textContent = text;
  status.classList.toggle("error", isError);
};

// Sends a request to the worker and waits for its reply.
function ask(message, transfer = []) {
  const reply = new Promise((resolve) => (worker.onmessage = ({ data }) => resolve(data)));
  worker.postMessage(message, transfer);
  return reply;
}

const fontFile = (name, bytes) => ({ name, bytes });
const tag = (bytes, offset) => String.fromCharCode(...bytes.subarray(offset, offset + 4));
const isCollection = (bytes) => tag(bytes, 0) === "ttcf";
// Whether a font file (its first face) has outlines FreeType can draw. macOS's PingFang, for
// one, only has Apple's own 'hvgl' outlines.
function hasOutlines(bytes) {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const face = isCollection(bytes) ? view.getUint32(12) : 0;
  const tableCount = view.getUint16(face + 4);
  return Array.from({ length: tableCount }, (_, index) => tag(bytes, face + 12 + 16 * index)).some(
    (table) => table === "glyf" || table === "CFF " || table === "CFF2",
  );
}
// The family part of a PostScript name ("HelveticaNeue-Bold" -> "HelveticaNeue").
const baseName = (postscriptName) => postscriptName.split("-")[0].toLowerCase();

// Installed faces the conversion may use, read once each: every weight and style of the families
// the document names, and exactly the fallback faces (CJK families are large).
async function sendLocalFonts(named, fallbacks) {
  const families = new Set(named.map(baseName));
  const exact = new Set(fallbacks);
  const faces = localFonts.filter(
    (face) => families.has(baseName(face.postscriptName)) || exact.has(face.postscriptName),
  );
  const files = [];
  const seenFiles = new Set();
  for (const face of faces) {
    const name = face.postscriptName;
    if (sentFonts.has(name)) continue;
    sentFonts.add(name);
    const blob = await face.blob();
    // Faces of one collection (.ttc) return the same file: send it once.
    const fileKey = `${baseName(name)}|${blob.size}`;
    if (seenFiles.has(fileKey)) continue;
    seenFiles.add(fileKey);
    const bytes = new Uint8Array(await blob.arrayBuffer());
    if (!hasOutlines(bytes)) continue;
    files.push(fontFile(`${name}.${isCollection(bytes) ? "ttc" : "ttf"}`, bytes));
  }
  if (files.length) {
    worker.postMessage({ type: "addFonts", files }, files.map((file) => file.bytes.buffer));
  }
}

async function convert(file) {
  if (!file || busy) return;
  const extension = file.name.split(".").pop().toLowerCase();
  if (!["pages", "numbers", "key"].includes(extension)) {
    showStatus(`${file.name} is not a .pages, .numbers or .key file.`, true);
    return;
  }
  busy = true;
  dropZone.setAttribute("aria-busy", "true");
  showStatus(`Converting ${file.name}...`);
  const started = performance.now();
  const bytes = new Uint8Array(await file.arrayBuffer());
  let fontNote = "";
  if (localFonts) {
    const listed = await ask({ type: "fontNames", bytes });
    if (listed.named) {
      await sendLocalFonts(listed.named, listed.fallbacks);
      const installed = new Set(localFonts.map((face) => baseName(face.postscriptName)));
      const found = listed.named.filter((name) => installed.has(baseName(name))).length;
      fontNote = ` ${found} of ${listed.named.length} fonts from this device.`;
    }
  }
  const data = await ask({ type: "convert", bytes, extension }, [bytes.buffer]);
  busy = false;
  dropZone.removeAttribute("aria-busy");
  if (data.error) {
    result.hidden = true;
    showStatus(`Could not convert ${file.name}: ${data.error}`, true);
    return;
  }
  if (pdfUrl) URL.revokeObjectURL(pdfUrl);
  pdfUrl = URL.createObjectURL(new Blob([data.pdf], { type: "application/pdf" }));
  const seconds = ((performance.now() - started) / 1000).toFixed(1);
  const pageWord = data.pages === 1 ? "page" : "pages";
  showStatus(`${file.name}: ${data.pages} ${pageWord} in ${seconds} s.${fontNote}`);
  preview.src = pdfUrl;
  download.href = pdfUrl;
  download.download = file.name.replace(/\.[^.]+$/, "") + ".pdf";
  result.hidden = false;
}

worker.onerror = (event) => {
  busy = false;
  dropZone.removeAttribute("aria-busy");
  showStatus(`The converter failed to load: ${event.message}`, true);
};

// Installed fonts: Chromium's Local Font Access API, which asks for permission on a click.
if ("queryLocalFonts" in window) {
  localFontsButton.hidden = false;
  localFontsButton.addEventListener("click", async () => {
    try {
      localFonts = await window.queryLocalFonts();
    } catch (error) {
      localFonts = null;
      fontStatus.textContent = `Font access was not granted (${error.message}); using Nimbus Sans.`;
      return;
    }
    if (!localFonts.length) {
      localFonts = null;
      fontStatus.textContent = "Font access was not granted; using Nimbus Sans.";
      return;
    }
    localFontsButton.disabled = true;
    fontStatus.textContent = `${localFonts.length} installed fonts available.`;
  });
}

// Font files the user picks (any browser): matched by their PostScript names.
fontFiles.addEventListener("change", async () => {
  const files = await Promise.all(
    [...fontFiles.files]
      .filter((file) => !sentFonts.has(file.name))
      .map(async (file) => fontFile(file.name, new Uint8Array(await file.arrayBuffer()))),
  );
  for (const file of files) sentFonts.add(file.name);
  worker.postMessage({ type: "addFonts", files }, files.map((file) => file.bytes.buffer));
  fontStatus.textContent = `${files.length} font files added.`;
});

picker.addEventListener("change", () => convert(picker.files[0]));
for (const type of ["dragenter", "dragover"]) {
  dropZone.addEventListener(type, (event) => {
    event.preventDefault();
    dropZone.classList.add("dragging");
  });
}
dropZone.addEventListener("dragleave", () => dropZone.classList.remove("dragging"));
dropZone.addEventListener("drop", (event) => {
  event.preventDefault();
  dropZone.classList.remove("dragging");
  convert(event.dataTransfer.files[0]);
});
