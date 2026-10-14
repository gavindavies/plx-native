//! Code generation for the platform layer: the `i18n::msg` localization catalog. Nothing here
//! links anything; the host link configuration (SDL, GL, the nanosvg object) belongs to the
//! application crate's build script, which owns the final binary.

use std::path::{Path, PathBuf};

#[path = "build_support/catalog.rs"]
mod catalog;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build_support/catalog.rs");
    println!("cargo:rerun-if-changed=../../locales");
    catalog::build(Path::new("../../locales"), &PathBuf::from(std::env::var_os("OUT_DIR").unwrap()))
        .expect("a valid localization catalog");
}
