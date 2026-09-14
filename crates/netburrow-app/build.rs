use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=assets/netburrow.ico");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let icon =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("assets/netburrow.ico");
    let resource = out.join("icon.rc");
    std::fs::write(
        &resource,
        format!(
            "1 ICON \"{}\"\n",
            icon.display().to_string().replace('\\', "/")
        ),
    )
    .unwrap();
    let compiled = out.join("icon.res");
    // Prefer the SDK selected by the developer command prompt, then installed SDKs.
    let compiler = if Command::new("rc.exe").arg("/?").output().is_ok() {
        PathBuf::from("rc.exe")
    } else {
        let kits =
            PathBuf::from(env::var_os("ProgramFiles(x86)").expect("Windows SDK is required"))
                .join("Windows Kits/10/bin");
        let mut candidates: Vec<_> = std::fs::read_dir(kits)
            .expect("Windows SDK is required")
            .filter_map(Result::ok)
            .map(|entry| entry.path().join("x64/rc.exe"))
            .filter(|path| path.is_file())
            .collect();
        candidates.sort();
        candidates
            .pop()
            .expect("Windows SDK resource compiler is required")
    };
    let status = Command::new(compiler)
        .arg("/nologo")
        .arg("/fo")
        .arg(&compiled)
        .arg(&resource)
        .status()
        .expect("Cannot run Windows resource compiler");
    assert!(status.success(), "Icon resource compilation failed");
    println!("cargo:rustc-link-arg-bin=NetBurrow={}", compiled.display());
}
