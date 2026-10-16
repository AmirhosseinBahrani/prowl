//! Local-only transport for the daemon API: a Unix domain socket on macOS/Linux,
//! a named pipe on Windows.

use std::fmt;
use std::io;
use std::path::Path;

/// Where the daemon listens. A socket path on Unix, a pipe name on Windows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint(String);

impl Endpoint {
    #[cfg(unix)]
    pub fn for_home(home: &Path) -> Self {
        Self(home.join("prowld.sock").display().to_string())
    }

    /// Pipe names are global, so derive a per-home name to let several daemons coexist (tests).
    #[cfg(windows)]
    pub fn for_home(home: &Path) -> Self {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        home.hash(&mut hasher);
        Self(format!(r"\\.\pipe\prowld-{:016x}", hasher.finish()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(unix)]
mod imp {
    use super::*;
    use tokio::net::{UnixListener, UnixStream};

    pub type Listener = UnixListener;
    pub type Stream = UnixStream;

    pub async fn bind(endpoint: &Endpoint) -> io::Result<Listener> {
        let path = Path::new(endpoint.as_str());
        if path.exists() {
            if UnixStream::connect(path).await.is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("another prowld is already listening on {endpoint}"),
                ));
            }
            std::fs::remove_file(path)?;
        }
        let listener = UnixListener::bind(path)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(listener)
    }

    pub async fn connect(endpoint: &Endpoint) -> io::Result<Stream> {
        UnixStream::connect(endpoint.as_str()).await
    }

    pub fn cleanup(endpoint: &Endpoint) {
        let _ = std::fs::remove_file(endpoint.as_str());
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::time::Duration;
    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };

    const ERROR_PIPE_BUSY: i32 = 231;

    pub type Stream = NamedPipeClient;

    /// Accepts connections by keeping one unconnected pipe instance ready at all times.
    pub struct Listener {
        name: String,
        next: NamedPipeServer,
    }

    pub async fn bind(endpoint: &Endpoint) -> io::Result<Listener> {
        let next = ServerOptions::new()
            .first_pipe_instance(true)
            .create(endpoint.as_str())
            .map_err(|e| {
                if e.kind() == io::ErrorKind::PermissionDenied {
                    io::Error::new(
                        io::ErrorKind::AddrInUse,
                        format!("another prowld is already listening on {endpoint}"),
                    )
                } else {
                    e
                }
            })?;
        Ok(Listener {
            name: endpoint.as_str().to_owned(),
            next,
        })
    }

    impl axum::serve::Listener for Listener {
        type Io = NamedPipeServer;
        type Addr = String;

        async fn accept(&mut self) -> (Self::Io, Self::Addr) {
            loop {
                if let Err(e) = self.next.connect().await {
                    tracing::warn!(error = %e, "named pipe connect failed");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
                match ServerOptions::new().create(&self.name) {
                    Ok(fresh) => {
                        let connected = std::mem::replace(&mut self.next, fresh);
                        return (connected, self.name.clone());
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "creating next named pipe instance failed");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
        }

        fn local_addr(&self) -> io::Result<Self::Addr> {
            Ok(self.name.clone())
        }
    }

    pub async fn connect(endpoint: &Endpoint) -> io::Result<Stream> {
        loop {
            match ClientOptions::new().open(endpoint.as_str()) {
                Ok(client) => return Ok(client),
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub fn cleanup(_endpoint: &Endpoint) {}
}

pub(crate) use imp::{Listener, bind, cleanup};
pub use imp::{Stream, connect};
