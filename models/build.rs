#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use fluorite_codegen::code_gen::rust::RustOptions;

fn main() {
    println!("cargo:rerun-if-changed=../fluorite");
    println!("cargo:rerun-if-changed=build.rs");

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let options = RustOptions::new(out_dir)
        .with_any_type("serde_json::Value")
        .with_single_file(true)
        .with_derives(vec![
            "Debug".to_string(),
            "Clone".to_string(),
            "PartialEq".to_string(),
            "serde::Serialize".to_string(),
            "serde::Deserialize".to_string(),
            "schemars::JsonSchema".to_string(),
        ]);

    fluorite_codegen::compile_with_options(options, &["../fluorite"])
        .expect("Failed to compile fluorite schemas");
}
