//! Compiles the Slint UI (`ui/app.slint` and its imports) into Rust.
//! The Fluent style is selected explicitly so the application keeps a
//! Windows-consistent look whatever the default for the platform is.
//! On Windows it also embeds the version-info resource, icon and
//! application manifest into `rsearch.exe`.

fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".to_string());
    slint_build::compile_with_config("ui/app.slint", config).expect("slint compile failed");

    #[cfg(windows)]
    embed_windows_resources();
}

/// Windows resources for `rsearch.exe`: VERSIONINFO metadata, the app
/// icon and a manifest. Unsigned binaries without metadata are flagged
/// much more often by AV heuristics and corporate filters — this does
/// not replace code signing, but removes the most common triggers.
#[cfg(windows)]
fn embed_windows_resources() {
    let mut res = winres::WindowsResource::new();
    res.set_icon("assets/app-icon.ico");
    res.set("CompanyName", "Frederik Benoist");
    res.set("ProductName", "rsearch");
    res.set("FileDescription", "rsearch - Fast Local Code Search Engine");
    res.set("LegalCopyright", "Copyright © 2026 Frederik Benoist");
    res.set("InternalName", "rsearch");
    res.set("OriginalFilename", "rsearch.exe");
    let version = std::env::var("CARGO_PKG_VERSION").expect("package version");
    res.set("FileVersion", &version);
    res.set("ProductVersion", &version);
    // Numeric FILEVERSION/PRODUCTVERSION: major.minor.patch.0 packed
    // 16 bits per field.
    let parts: Vec<u64> = version.split('.').map(|p| p.parse().unwrap_or(0)).collect();
    if let [major, minor, patch] = parts[..] {
        let packed = (major << 48) | (minor << 32) | (patch << 16);
        res.set_version_info(winres::VersionInfo::FILEVERSION, packed);
        res.set_version_info(winres::VersionInfo::PRODUCTVERSION, packed);
    }
    // asInvoker manifest + declared OS support; no DPI entry — winit
    // manages DPI awareness at runtime and a manifest would lock it.
    res.set_manifest(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity version="1.0.0.0" name="rsearch" type="win32"/>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
      <supportedOS Id="{1f676c76-80e1-4239-95bb-83d0f6d0da78}"/>
      <supportedOS Id="{4a2f28e3-53b9-4441-ba9c-d69d4a4a6e38}"/>
      <supportedOS Id="{35138b9a-5d96-4fbd-8e2d-a2440225f93a}"/>
    </application>
  </compatibility>
</assembly>"#,
    );
    res.compile().expect("windows resource compile failed");
}
