fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Try system protoc first, fall back to a local install (mirrors the
    // service/gui build scripts).
    if std::process::Command::new("protoc").arg("--version").output().is_err() {
        let local_protoc =
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
                .join(".local/protoc/bin/protoc");
        if local_protoc.exists() {
            std::env::set_var("PROTOC", &local_protoc);
        }
    }
    // Client only — we never serve from here.
    tonic_build::configure()
        .build_server(false)
        .compile_protos(&["../../proto/ownbackup.proto"], &["../../proto"])?;
    Ok(())
}
