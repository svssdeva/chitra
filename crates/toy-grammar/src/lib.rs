//! A grammar the binary has never heard of, exported under a C symbol.
//!
//! It is Python's grammar underneath — the point is not the language, it is that
//! `chitra` reaches it purely through `languages.toml` and `dlopen`, with no
//! Rust change and no compiled-in knowledge of the name `toy`.

/// # Safety
/// Tree-sitter grammar entry point: returns a pointer to a static `TSLanguage`.
#[no_mangle]
pub unsafe extern "C" fn tree_sitter_toy() -> *const () {
    (tree_sitter_python::LANGUAGE.into_raw())()
}
