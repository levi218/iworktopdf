//! Emscripten link flags: a modular JS loader exporting the C entry points, with font metrics and
//! the Nimbus Sans faces (the browser has no installed fonts) embedded in the module's file system.

fn main() {
    // Build scripts see the target through CARGO_CFG_* variables (`cfg!` here describes the host).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("emscripten") {
        return;
    }
    let fonts = concat!(env!("CARGO_MANIFEST_DIR"), "/../iwork2pdf/fonts");
    let embedded = [
        format!("{fonts}/apple-metrics.json@/res/apple-metrics.json"),
        format!("{fonts}/open/nimbus-sans@/res/open/nimbus-sans"),
    ];
    let flags = [
        "-sMODULARIZE=1",
        "-sEXPORT_NAME=createConverter",
        "-sENVIRONMENT=web,worker",
        "-sALLOW_MEMORY_GROWTH=1",
        "-sSTACK_SIZE=8MB",
        // Skia is C++: link libc++, and leave the C++ runtime stubs it references unresolved.
        "-sDEFAULT_TO_CXX",
        "-sERROR_ON_UNDEFINED_SYMBOLS=0",
        "-sEXPORTED_FUNCTIONS=_main,_malloc,_free,_convert,_font_names,_output_ptr,_output_len",
        "-sEXPORTED_RUNTIME_METHODS=ccall,HEAPU8,FS",
    ];
    for flag in flags {
        println!("cargo:rustc-link-arg-bins={flag}");
    }
    for file in embedded {
        println!("cargo:rustc-link-arg-bins=--embed-file");
        println!("cargo:rustc-link-arg-bins={file}");
    }
}
