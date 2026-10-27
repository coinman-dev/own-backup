//! The service, reached the way the window reaches it: the named pipe with
//! the service's token (loopback TCP for a development service run by hand),
//! the calls made blocking on a runtime of the plugin's own — Total
//! Commander calls a plugin from its own threads and waits for the answer.

use serde_json::{json, Value};
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::Request;

use crate::browse::{Entry, Opened, Remote, RemoteError, SourceInfo};
use crate::proto::backup_service_client::BackupServiceClient;
use crate::proto::JsonRequest;

/// What the service says when a session has gone (service/src/linux_browse.rs).
const SESSION_GONE: &str = "linux-browse: no such session";

pub struct Grpc {
    rt: tokio::runtime::Runtime,
    client: Option<(BackupServiceClient<Channel>, String)>,
}

impl Grpc {
    pub fn new() -> std::io::Result<Self> {
        Ok(Self {
            rt: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?,
            client: None,
        })
    }

    fn connect(&mut self) -> Result<(), RemoteError> {
        if self.client.is_some() {
            return Ok(());
        }
        let config = own_backup_common::config::AppConfig::load_or_default();
        let mut token = std::fs::read_to_string(config.token_path())
            .map(|t| t.trim().to_string())
            .unwrap_or_default();
        let mut endpoint = format!("http://{}", config.grpc_address);
        let mut pipe = true;
        // Development builds: a service elsewhere (`OWN_BACKUP_ENDPOINT=
        // http://127.0.0.1:50051`, a WSL one), never the installed one's pipe.
        if cfg!(debug_assertions) {
            if let Ok(e) = std::env::var("OWN_BACKUP_ENDPOINT") {
                endpoint = e;
                pipe = false;
                token = std::env::var("OWN_BACKUP_TOKEN").unwrap_or_default();
            }
        }
        let channel = self
            .rt
            .block_on(connect(endpoint, pipe))
            .map_err(|e| RemoteError::Failed(own_backup_common::tr!("tc.no_service", error = e).into_owned()))?;
        self.client = Some((
            BackupServiceClient::new(channel)
                .max_decoding_message_size(own_backup_common::MAX_RPC_MESSAGE)
                .max_encoding_message_size(own_backup_common::MAX_RPC_MESSAGE),
            token,
        ));
        Ok(())
    }

    fn request(token: &str, json: String) -> Request<JsonRequest> {
        let mut req = Request::new(JsonRequest { json });
        if let Ok(v) = MetadataValue::try_from(format!("Bearer {token}")) {
            req.metadata_mut().insert("authorization", v);
        }
        req
    }

    fn status(&mut self, s: tonic::Status) -> RemoteError {
        if s.message() == SESSION_GONE {
            return RemoteError::SessionGone;
        }
        // The connection may be what failed: make a new one next time.
        if matches!(
            s.code(),
            tonic::Code::Unavailable | tonic::Code::Unknown | tonic::Code::Unauthenticated
        ) {
            self.client = None;
        }
        RemoteError::Failed(s.message().to_string())
    }

    fn call(&mut self, op: Value) -> Result<Value, RemoteError> {
        self.connect()?;
        let (client, token) = self.client.as_mut().expect("connected above");
        let req = Self::request(token, op.to_string());
        let mut client = client.clone();
        match self.rt.block_on(client.linux_browse(req)) {
            Ok(reply) => serde_json::from_str(&reply.into_inner().json)
                .map_err(|e| RemoteError::Failed(e.to_string())),
            Err(s) => Err(self.status(s)),
        }
    }

    fn parse<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, RemoteError> {
        serde_json::from_value(v).map_err(|e| RemoteError::Failed(e.to_string()))
    }
}

impl Remote for Grpc {
    fn sources(&mut self) -> Result<Vec<SourceInfo>, RemoteError> {
        Self::parse(self.call(json!({ "op": "sources" }))?)
    }

    fn open(&mut self, source: &str, passphrase: Option<&str>) -> Result<Opened, RemoteError> {
        Self::parse(self.call(json!({ "op": "open", "source": source, "passphrase": passphrase }))?)
    }

    fn list(&mut self, session: u64, root: usize, path: &str) -> Result<Vec<Entry>, RemoteError> {
        Self::parse(self.call(
            json!({ "op": "list", "session": session, "root": root, "path": path }),
        )?)
    }

    fn stat(&mut self, session: u64, root: usize, path: &str) -> Result<Entry, RemoteError> {
        Self::parse(self.call(
            json!({ "op": "stat", "session": session, "root": root, "path": path }),
        )?)
    }

    fn read(
        &mut self,
        session: u64,
        root: usize,
        path: &str,
        offset: u64,
        len: usize,
    ) -> Result<Vec<u8>, RemoteError> {
        self.connect()?;
        let (client, token) = self.client.as_mut().expect("connected above");
        let req = Self::request(
            token,
            json!({ "session": session, "root": root, "path": path, "offset": offset, "len": len })
                .to_string(),
        );
        let mut client = client.clone();
        match self.rt.block_on(client.linux_browse_read(req)) {
            Ok(reply) => Ok(reply.into_inner().data),
            Err(s) => Err(self.status(s)),
        }
    }
}

/// The pipe first; loopback TCP when there is no pipe (a development
/// service run with `--foreground`).
async fn connect(endpoint: String, pipe: bool) -> Result<Channel, String> {
    #[cfg(windows)]
    if pipe {
        if let Ok(channel) = connect_pipe().await {
            return Ok(channel);
        }
    }
    #[cfg(not(windows))]
    let _ = pipe;
    tonic::transport::Endpoint::try_from(endpoint)
        .map_err(|e| e.to_string())?
        .connect_timeout(std::time::Duration::from_millis(800))
        .connect()
        .await
        .map_err(|e| e.to_string())
}

/// As `crates/client` connects: the narrow access mask, a retry while every
/// pipe instance is busy.
#[cfg(windows)]
async fn connect_pipe() -> Result<Channel, String> {
    use hyper_util::rt::TokioIo;
    use tonic::transport::{Endpoint, Uri};
    use tower::service_fn;

    const ERROR_PIPE_BUSY: i32 = 231;
    let pipe_path = own_backup_common::windows_pipe_path();
    Endpoint::try_from("http://ownbackup.pipe")
        .map_err(|e| e.to_string())?
        .connect_timeout(std::time::Duration::from_millis(800))
        .connect_with_connector(service_fn(move |_: Uri| {
            let pipe_path = pipe_path.clone();
            async move {
                for _ in 0..20u32 {
                    match open_service_pipe(&pipe_path) {
                        Ok(client) => return Ok(TokioIo::new(client)),
                        Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        }
                        Err(e) => return Err(e),
                    }
                }
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "named pipe busy (all instances)",
                ))
            }
        }))
        .await
        .map_err(|e| e.to_string())
}

#[cfg(windows)]
fn open_service_pipe(
    path: &str,
) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    use tokio::net::windows::named_pipe::NamedPipeClient;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_MODE, OPEN_EXISTING, SECURITY_IDENTIFICATION,
        SECURITY_SQOS_PRESENT,
    };

    // FILE_GENERIC_READ | FILE_GENERIC_WRITE, minus FILE_APPEND_DATA (0x4):
    // the pipe's DACL refuses clients that could create instances.
    const CLIENT_ACCESS: u32 = 0x0012_019B;
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: a terminated path; the handle returned is owned below.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            CLIENT_ACCESS,
            FILE_SHARE_MODE(0),
            None,
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED | SECURITY_IDENTIFICATION | SECURITY_SQOS_PRESENT,
            None,
        )
    }
    .map_err(|_| std::io::Error::last_os_error())?;
    // SAFETY: one owned handle opened for overlapped duplex I/O; tokio closes it.
    unsafe { NamedPipeClient::from_raw_handle(handle.0) }
}
