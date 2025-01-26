use std::{env, fs::read_dir, path::PathBuf};

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    let target = env::var("TARGET").unwrap();
    let tree_sitter_path = PathBuf::from("tree-sitter-ng");
    let src_path = tree_sitter_path.join("tree-sitter/src/main/c");
    let mut sources: Vec<PathBuf> = Vec::new();
    for entry in read_dir(&src_path).unwrap() {
        let entry = entry.unwrap();
        let path = src_path.join(entry.file_name());
        match path.extension().and_then(|e| e.to_str()) {
            Some("c") => {
                sources.push(path.clone());
            }
            Some("h") => (),
            _ => {
                continue;
            }
        }
        println!("cargo::rerun-if-changed={}", path.to_str().unwrap());
    }
    let jni_md_subdir = if target.contains("windows") {
        "win32"
    } else if target.contains("linux") {
        "linux"
    } else if target.contains("darwin") {
        "darwin"
    } else {
        panic!("target {target} is not supported");
    };
    cc::Build::new()
        .define("JNI_ONLOAD_NAME", Some("tree_sitter_ng_JNI_OnLoad"))
        .flag_if_supported("-Wno-implicit-fallthrough")
        .flag_if_supported("-Wno-unused-parameter")
        .include(tree_sitter_path.join("include/jni"))
        .include(tree_sitter_path.join("include/jni").join(jni_md_subdir))
        .include("treesitter_include")
        .files(sources)
        .compile("tree-sitter-ng");
}
