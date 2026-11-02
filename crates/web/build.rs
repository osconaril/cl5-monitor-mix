use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=src/shell.html");
    let output = PathBuf::from(env::var("OUT_DIR").expect("Cargo sets OUT_DIR")).join("index.html");
    fs::write(output, include_str!("src/shell.html")).expect("write generated web shell");
}
