fn main() -> Result<(), Box<dyn std::error::Error>> {
    tauri_build::build();

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
