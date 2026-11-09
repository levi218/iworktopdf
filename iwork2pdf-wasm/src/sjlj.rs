//! setjmp/longjmp for the prebuilt Skia (FreeType, libpng, libjpeg use them for error recovery).
//!
//! Those C objects were compiled for Emscripten's JavaScript-based setjmp/longjmp: every call that
//! may longjmp goes through an `invoke_<signature>(function, args...)` import, and `longjmp` is
//! `emscripten_longjmp`. Rust's emscripten target links with native wasm exceptions instead, and
//! Emscripten refuses to provide the JavaScript `invoke_*` helpers then. They are defined here, in
//! the module itself: a longjmp becomes a Rust unwind (a wasm exception, which passes through C
//! frames untouched) that the nearest `invoke_*` catches, recording the jump the way the
//! JavaScript helpers do (`setThrew`) so the compiled C code resumes at its `setjmp`.

use std::panic::{self, AssertUnwindSafe};

extern "C" {
    /// Records a pending longjmp in `__THREW__` / `__threwValue` (Emscripten's libc).
    fn setThrew(threw: usize, value: i32);
    /// The C shadow stack pointer, saved before a call that may longjmp.
    fn emscripten_stack_get_current() -> usize;
    /// Resets the C shadow stack pointer (unwound C frames do not pop their own).
    fn _emscripten_stack_restore(stack: usize);
}

/// The unwind payload of a longjmp.
struct LongJump;

/// `longjmp(env, value)` as compiled C code calls it.
///
/// # Safety
///
/// Called only by Emscripten-compiled C code, from inside an `invoke_*` call.
#[no_mangle]
pub unsafe extern "C-unwind" fn emscripten_longjmp(env: usize, value: i32) -> ! {
    // C: longjmp never makes setjmp return 0.
    unsafe { setThrew(env, if value == 0 { 1 } else { value }) };
    // `resume_unwind` unwinds without running the panic hook (no "panicked at" message).
    panic::resume_unwind(Box::new(LongJump))
}

/// Defines `invoke_<signature>`: calls the function at a table index, catching a longjmp.
/// (A macro because the 16 signatures differ only in arity and return type.)
macro_rules! invoke {
    ($($name:ident($($arg:ident),*) -> $ret:ty;)*) => {$(
        /// Calls `function` with the arguments; a longjmp out of it is recorded for the caller.
        ///
        /// # Safety
        ///
        /// Called only by Emscripten-compiled C code with a valid function table index.
        #[no_mangle]
        pub unsafe extern "C-unwind" fn $name(
            function: unsafe extern "C-unwind" fn($($arg: i32),*) -> $ret,
            $($arg: i32),*
        ) -> $ret {
            let stack = unsafe { emscripten_stack_get_current() };
            match panic::catch_unwind(AssertUnwindSafe(|| unsafe { function($($arg),*) })) {
                Ok(result) => result,
                Err(payload) if payload.is::<LongJump>() => {
                    unsafe {
                        _emscripten_stack_restore(stack);
                        // `setThrew` keeps the target recorded by `emscripten_longjmp`.
                        setThrew(1, 0);
                    }
                    // The caller reads `__THREW__` and ignores the result.
                    <$ret>::default()
                }
                Err(payload) => panic::resume_unwind(payload),
            }
        }
    )*};
}

invoke! {
    invoke_v() -> ();
    invoke_vi(a) -> ();
    invoke_vii(a, b) -> ();
    invoke_viii(a, b, c) -> ();
    invoke_viiii(a, b, c, d) -> ();
    invoke_viiiii(a, b, c, d, e) -> ();
    invoke_viiiiii(a, b, c, d, e, f) -> ();
    invoke_viiiiiiiii(a, b, c, d, e, f, g, h, i) -> ();
    invoke_ii(a) -> i32;
    invoke_iii(a, b) -> i32;
    invoke_iiii(a, b, c) -> i32;
    invoke_iiiii(a, b, c, d) -> i32;
    invoke_iiiiii(a, b, c, d, e) -> i32;
    invoke_iiiiiii(a, b, c, d, e, f) -> i32;
    invoke_iiiiiiii(a, b, c, d, e, f, g) -> i32;
    invoke_iiiiiiiiii(a, b, c, d, e, f, g, h, i) -> i32;
}
