// Runs the WebAssembly converter off the main thread so the page stays responsive.
importScripts("iwork2pdf-wasm.js");

// Supplied font files by file name; written to the module's /fonts before converting.
const fonts = new Map();
const written = new Set();
let module = null;
let converted = false;

// The library indexes /fonts once per module (at its first conversion), so fonts added after a
// conversion need a fresh module.
async function moduleWithFonts() {
  const unwritten = [...fonts.keys()].some((name) => !written.has(name));
  if (!module || (converted && unwritten)) {
    module = await createConverter();
    converted = false;
    written.clear();
  }
  for (const [name, bytes] of fonts) {
    if (!written.has(name)) {
      module.FS.writeFile(`/fonts/${name}`, bytes);
      written.add(name);
    }
  }
  return module;
}

// Calls a C export on the document's bytes; returns its count and output bytes.
function call(name, bytes, extraTypes = [], extraArgs = []) {
  const input = module._malloc(bytes.length);
  module.HEAPU8.set(bytes, input);
  const count = module.ccall(
    name,
    "number",
    ["number", "number", ...extraTypes],
    [input, bytes.length, ...extraArgs],
  );
  module._free(input);
  // HEAPU8 is re-created when memory grows, so read it only after the call.
  const start = module._output_ptr();
  return { count, output: module.HEAPU8.slice(start, start + module._output_len()) };
}

const handlers = {
  addFonts({ files }) {
    for (const { name, bytes } of files) fonts.set(name, bytes);
  },
  async fontNames({ bytes }) {
    await moduleWithFonts();
    const { count, output } = call("font_names", bytes);
    const text = new TextDecoder().decode(output);
    if (count < 0) {
      postMessage({ error: text });
      return;
    }
    // Named fonts, an empty line, then fallback faces.
    const [named, fallbacks] = text.split("\n\n").map((part) => (part ? part.split("\n") : []));
    postMessage({ named, fallbacks });
  },
  async convert({ bytes, extension }) {
    await moduleWithFonts();
    converted = true;
    const { count, output } = call("convert", bytes, ["string"], [extension]);
    if (count < 0) {
      postMessage({ error: new TextDecoder().decode(output) });
    } else {
      postMessage({ pdf: output, pages: count }, [output.buffer]);
    }
  },
};

// Messages are handled one at a time, in order (the page awaits each reply).
let queue = Promise.resolve();
onmessage = ({ data }) => {
  queue = queue
    .then(() => handlers[data.type](data))
    .catch((error) => postMessage({ error: String(error) }));
};
