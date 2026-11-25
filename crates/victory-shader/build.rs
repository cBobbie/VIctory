//! A fingerprint of the translator's own source and enabled features,
//! `VICTORY_SHADER_SOURCE`: the shader cache keys its entries by it, so a
//! rebuild that leaves the translator alone keeps its translations.

use std::path::{Path, PathBuf};

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// FNV-1a over `bytes`, from `seed`.
fn fnv(mut h: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn main() {
    let root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("set by cargo"));
    let mut all = Vec::new();
    files(&root.join("src"), &mut all);
    all.push(root.join("Cargo.toml"));
    all.sort();
    // Two seeds: 128 bits, so two sources never meet by chance.
    let mut h = [0xcbf2_9ce4_8422_2325_u64, 0x6c62_272e_07bb_0142];
    for path in &all {
        let rel = path.strip_prefix(&root).unwrap_or(path).to_string_lossy().replace('\\', "/");
        let bytes = std::fs::read(path).unwrap_or_default();
        for x in &mut h {
            *x = fnv(fnv(*x, rel.as_bytes()), &bytes);
        }
    }
    // The features change the translation too.
    let mut features: Vec<String> = std::env::vars().map(|(k, _)| k).filter(|k| k.starts_with("CARGO_FEATURE_")).collect();
    features.sort();
    for f in &features {
        for x in &mut h {
            *x = fnv(*x, f.as_bytes());
        }
    }
    println!("cargo:rustc-env=VICTORY_SHADER_SOURCE={:016x}{:016x}", h[0], h[1]);
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
}
