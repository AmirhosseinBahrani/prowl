//! Local-only transport for the daemon API: a Unix domain socket on macOS/Linux,
//! a named pipe on Windows.

use std::fmt;
use std::io;
use std::path::Path;

/// Where the daemon listens. A socket path on Unix, a pipe name on Windows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint(String);

impl Endpoint {
    /// `<home>/prowld.sock`, unless that path is too long for a Unix socket (about 104 bytes);
    /// then a socket named after the home inside a private per-user runtime directory.
    #[cfg(unix)]
    pub fn for_home(home: &Path) -> Self {
        let beside_home = home.join("prowld.sock");
        if beside_home.as_os_str().len() <= MAX_SOCKET_PATH {
            return Self(beside_home.display().to_string());
        }
        Self(
            imp::runtime_dir()
                .join(format!("{:016x}.sock", stable_hash(home)))
                .display()
                .to_string(),
        )
    }

    /// Pipe names are global, so derive a per-home name to let several daemons coexist (tests).
    #[cfg(windows)]
    pub fn for_home(home: &Path) -> Self {
        Self(format!(r"\\.\pipe\prowld-{:016x}", stable_hash(home)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Below `sun_path`'s 104 bytes on macOS (108 on Linux), with room for the terminator.
#[cfg(unix)]
const MAX_SOCKET_PATH: usize = 100;

/// FNV-1a: stable across builds, unlike `DefaultHasher`, so the CLI and the daemon always
/// agree on the endpoint.
fn stable_hash(path: &Path) -> u64 {
    path.as_os_str()
        .as_encoded_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
        })
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

    /// `$XDG_RUNTIME_DIR/prowl` when available, else `/tmp/prowl-<uid>`.
    pub fn runtime_dir() -> std::path::PathBuf {
        match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(dir) if !dir.is_empty() => Path::new(&dir).join("prowl"),
            // SAFETY: getuid has no preconditions and cannot fail.
            _ => Path::new("/tmp").join(format!("prowl-{}", unsafe { libc::getuid() })),
        }
    }

    /// Creates the socket's directory if needed, refusing one another user could tamper with.
    fn ensure_private_dir(dir: &Path) -> io::Result<()> {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
        match std::fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        let meta = std::fs::symlink_metadata(dir)?;
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        if !meta.is_dir() || meta.uid() != uid || meta.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} must be a directory owned by you with mode 0700",
                    dir.display()
                ),
            ));
        }
        Ok(())
    }

    pub async fn bind(endpoint: &Endpoint) -> io::Result<Listener> {
        let path = Path::new(endpoint.as_str());
        if let Some(dir) = path.parent()
            && dir.starts_with(runtime_dir())
        {
            ensure_private_dir(dir)?;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_is_stable_per_home() {
        let a = Endpoint::for_home(Path::new("/home/a/prowl"));
        assert_eq!(a, Endpoint::for_home(Path::new("/home/a/prowl")));
        assert_ne!(a, Endpoint::for_home(Path::new("/home/b/prowl")));
    }

    #[cfg(unix)]
    #[test]
    fn long_homes_get_a_short_socket_path() {
        let long = Path::new("/Users/someone").join("x".repeat(150));
        let endpoint = Endpoint::for_home(&long);
        assert!(endpoint.as_str().len() <= MAX_SOCKET_PATH, "{endpoint}");
        assert!(endpoint.as_str().ends_with(".sock"));

        let short = Endpoint::for_home(Path::new("/home/a/prowl"));
        assert_eq!(short.as_str(), "/home/a/prowl/prowld.sock");
    }
}
