use std::{env, path::PathBuf, process::Command};

fn main() {
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR must be set"));
    let archive = out.join("libwhatsapp_bridge.a");

    let status = Command::new("go")
        .current_dir("go")
        .env("CGO_ENABLED", "1")
        .args(["build", "-buildmode=c-archive", "-o"])
        .arg(&archive)
        .arg(".")
        .status()
        .expect("failed to execute go build");

    assert!(status.success(), "go build failed");

    println!("cargo:rerun-if-changed=go/go.mod");
    println!("cargo:rerun-if-changed=go/bridge.go");
    println!("cargo:rerun-if-changed=go/main.go");
    println!("cargo:rerun-if-changed=go/lib.go");
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=whatsapp_bridge");

    #[cfg(target_os = "linux")]
    {
        println!("cargo:rustc-link-lib=pthread");
        println!("cargo:rustc-link-lib=dl");
    }

    #[cfg(target_os = "macos")]
    {
        println!("cargo:rustc-link-lib=framework=CoreFoundation");
        println!("cargo:rustc-link-lib=framework=Security");
    }
}
