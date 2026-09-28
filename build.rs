use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

fn assert_version_consistency(root: &Path) {
    let version = env::var("CARGO_PKG_VERSION").expect("Cargo package version");
    let parts = version
        .split('.')
        .map(|part| part.parse::<u32>().expect("numeric Cargo package version"))
        .collect::<Vec<_>>();
    assert_eq!(
        parts.len(),
        3,
        "Cargo package version must have three numeric parts"
    );
    let comma_version = format!("{},{},{},0", parts[0], parts[1], parts[2]);
    let dotted_version = format!("{version}.0");
    let resource = fs::read_to_string(root.join("assets/app.rc")).expect("read app.rc");
    assert!(
        resource.contains(&format!("FILEVERSION {comma_version}"))
            && resource.contains(&format!("PRODUCTVERSION {comma_version}"))
            && resource.contains(&format!("\"FileVersion\", \"{dotted_version}\\0\""))
            && resource.contains(&format!("\"ProductVersion\", \"{dotted_version}\\0\"")),
        "assets/app.rc version metadata does not match Cargo package version {version}"
    );
    let manifest = fs::read_to_string(root.join("assets/app.manifest")).expect("read app.manifest");
    assert!(
        manifest.contains(&format!("version=\"{dotted_version}\"")),
        "assets/app.manifest version does not match Cargo package version {version}"
    );
}

fn resource_compiler() -> PathBuf {
    if let Some(path) = env::var_os("SWITCHMUT_RC") {
        return path.into();
    }
    if Command::new("rc.exe").arg("/?").output().is_ok() {
        return "rc.exe".into();
    }
    let sdk = env::var_os("WindowsSdkDir").map(PathBuf::from).or_else(|| {
        env::var_os("ProgramFiles(x86)").map(|path| PathBuf::from(path).join("Windows Kits/10"))
    });
    if let Some(sdk) = sdk {
        let mut compilers: Vec<_> = std::fs::read_dir(sdk.join("bin"))
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let version = entry
                    .file_name()
                    .to_str()?
                    .split('.')
                    .map(str::parse::<u32>)
                    .collect::<Result<Vec<_>, _>>()
                    .ok()?;
                let path = entry.path().join("x64/rc.exe");
                path.is_file().then_some((version, path))
            })
            .collect();
        compilers.sort_by(|a, b| a.0.cmp(&b.0));
        if let Some((_, path)) = compilers.pop() {
            return path;
        }
    }
    panic!("Install the Windows SDK or set SWITCHMUT_RC to its rc.exe path");
}

fn main() {
    let root = std::env::var("CARGO_MANIFEST_DIR").expect("Cargo manifest directory");
    let root_path = PathBuf::from(&root);
    assert_version_consistency(&root_path);
    let resource =
        PathBuf::from(env::var_os("OUT_DIR").expect("Cargo output directory")).join("app.res");
    println!("cargo:rerun-if-env-changed=SWITCHMUT_RC");
    println!("cargo:rerun-if-env-changed=WindowsSdkDir");
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=assets/app.manifest");
    let status = Command::new(resource_compiler())
        .args(["/nologo", "/fo"])
        .arg(&resource)
        .arg("assets/app.rc")
        .current_dir(&root_path)
        .status()
        .expect("Run the Windows resource compiler");
    assert!(status.success(), "Windows icon resource compilation failed");
    println!("cargo:rustc-link-arg-bin=Switchmut={}", resource.display());
    println!("cargo:rustc-link-arg-bin=Switchmut=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bin=Switchmut=/MANIFESTINPUT:{root}/assets/app.manifest");
    println!("cargo:rustc-link-arg-bin=Switchmut=/MANIFESTUAC:NO");
    println!("cargo:rustc-link-arg-bin=Switchmut=/Brepro");
}
