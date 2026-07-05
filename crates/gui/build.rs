/// Application manifest for the Windows executable.
///
/// The key line is `requestedExecutionLevel level="asInvoker"`: the GUI must run
/// as the normal user and never prompt for UAC — all privileged work happens in
/// the backup service. We also declare Common-Controls v6 and per-monitor DPI,
/// which a GUI needs, since providing a custom manifest replaces Tauri's default.
const APP_MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false" />
      </requestedPrivileges>
    </security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}" />
      <supportedOS Id="{1f676c76-80e1-4239-95bb-83d0f6d0da78}" />
    </application>
  </compatibility>
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls"
        version="6.0.0.0" processorArchitecture="*"
        publicKeyToken="6595b64144ccf1df" language="*" />
    </dependentAssembly>
  </dependency>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
    </windowsSettings>
  </application>
</assembly>
"#;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Embed our own manifest so the exe runs as the invoking user (no UAC).
    tauri_build::try_build(
        tauri_build::Attributes::new().windows_attributes(
            tauri_build::WindowsAttributes::new().app_manifest(APP_MANIFEST),
        ),
    )
    .expect("failed to run tauri-build");

    // Proto compilation for gRPC client (reuses same proto as service)
    if std::process::Command::new("protoc")
        .arg("--version")
        .output()
        .is_err()
    {
        let local_protoc =
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
                .join(".local/protoc/bin/protoc");
        if local_protoc.exists() {
            std::env::set_var("PROTOC", &local_protoc);
        }
    }
    tonic_build::compile_protos("../../proto/ownbackup.proto")?;
    Ok(())
}
