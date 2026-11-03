// Вгражда иконата (assets/icon.ico) в exe-то, за да се вижда в Explorer и в лентата на задачите.
use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("app_res.o");
    let ok = Command::new("windres")
        .args(["--input-format=rc", "--output-format=coff", "-I", "assets", "-i", "assets/app.rc", "-o"])
        .arg(&out)
        .status()
        .is_ok_and(|s| s.success());
    if ok {
        println!("cargo:rustc-link-arg-bins={}", out.display());
    } else {
        println!("cargo:warning=windres не е намерен — exe-то ще е без икона");
    }
}
