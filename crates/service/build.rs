fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Try system protoc first, fall back to local install
    if std::process::Command::new("protoc").arg("--version").output().is_err() {
        let local_protoc = std::path::PathBuf::from(env!("HOME"))
            .join(".local/protoc/bin/protoc");
        if local_protoc.exists() {
            std::env::set_var("PROTOC", &local_protoc);
        }
    }
    tonic_build::compile_protos("../../proto/ownbackup.proto")?;
    Ok(())
}
