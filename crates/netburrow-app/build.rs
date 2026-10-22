use std::{env, io::Write, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=assets/netburrow.ico");
    embed_licenses();
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

/// License texts ship compressed inside the executable instead of as loose files in the package;
/// About -> Open source licenses writes them out for reading.
fn embed_licenses() {
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("../..");
    let mut text = String::new();
    for name in ["LICENSE", "THIRD_PARTY_LICENSES.txt"] {
        let path = root.join(name);
        println!("cargo:rerun-if-changed={}", path.display());
        text.push_str(&std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("Cannot read {name}: {error}")));
        text.push_str("\n\n");
    }
    let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
    encoder.write_all(text.as_bytes()).unwrap();
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    std::fs::write(out.join("licenses.deflate"), encoder.finish().unwrap()).unwrap();
}
