//! Host-local channel from the provider to envd inside a machine.
//!
//! Whatever the transport, envd speaks the ordinary data protocol over a
//! WebSocket. The preferred channel is a host AF_UNIX socket that smolvm
//! bridges over vsock to envd's guest socket (`--expose-socket`); it lives in
//! the provider's private per-machine directory, so only the provider account
//! can connect, and the guest needs no network for it.

use std::{
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Context as _;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::{WebSocketStream, client_async};

use crate::{config::EnvdChannel, store::TargetRecord};

/// File name of envd's host socket inside the machine directory.
pub const ENVD_SOCKET: &str = "envd.sock";

pub trait GuestIo: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> GuestIo for T {}

pub type GuestSocket = WebSocketStream<Box<dyn GuestIo>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GuestEndpoint {
    /// Host end of an `--expose-socket` bridge.
    HostSocket(PathBuf),
    /// A guest port published on the host.
    Loopback(SocketAddr),
}

/// Host endpoint for a record's envd channel.
pub fn endpoint(record: &TargetRecord, vm_dir: &Path) -> anyhow::Result<GuestEndpoint> {
    match record.envd {
        EnvdChannel::ExposedSocket { .. } => {
            Ok(GuestEndpoint::HostSocket(vm_dir.join(ENVD_SOCKET)))
        }
        EnvdChannel::LoopbackForward { .. } => {
            let port = record
                .host_port
                .context("loopback channel has no allocated host port")?;
            Ok(GuestEndpoint::Loopback(SocketAddr::from((
                Ipv4Addr::LOCALHOST,
                port,
            ))))
        }
    }
}

/// Pick a free loopback port for a published envd port.
///
/// The port is released before smolvm binds it, so another process could
/// take it in between; creation then fails and the retry picks a new port.
pub fn allocate_loopback_port() -> anyhow::Result<u16> {
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    Ok(listener.local_addr()?.port())
}

pub async fn dial(endpoint: &GuestEndpoint, timeout: Duration) -> anyhow::Result<GuestSocket> {
    tokio::time::timeout(timeout, async {
        let io: Box<dyn GuestIo> = match endpoint {
            GuestEndpoint::Loopback(address) => {
                Box::new(tokio::net::TcpStream::connect(address).await?)
            }
            GuestEndpoint::HostSocket(path) => connect_host_socket(path).await?,
        };
        let (socket, _) = client_async("ws://envd/", io).await?;
        anyhow::Ok(socket)
    })
    .await
    .context("envd dial timeout")?
}

#[cfg(unix)]
async fn connect_host_socket(path: &Path) -> anyhow::Result<Box<dyn GuestIo>> {
    Ok(Box::new(tokio::net::UnixStream::connect(path).await?))
}

/// smolvm bridges to an AF_UNIX path on Windows too (native since Windows 10
/// 1809), but tokio has no AF_UNIX client there.
#[cfg(windows)]
async fn connect_host_socket(path: &Path) -> anyhow::Result<Box<dyn GuestIo>> {
    anyhow::bail!(
        "AF_UNIX client for {} is not implemented on Windows yet",
        path.display()
    )
}

pub async fn probe(endpoint: &GuestEndpoint, timeout: Duration) -> bool {
    match dial(endpoint, timeout).await {
        Ok(mut socket) => {
            let _ = socket.close(None).await;
            true
        }
        Err(_) => false,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use futures_util::{SinkExt as _, StreamExt as _};
    use tokio_tungstenite::tungstenite::Message;

    use super::*;

    #[tokio::test]
    async fn dials_envd_over_a_host_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(ENVD_SOCKET);
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let envd = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let message = socket.next().await.unwrap().unwrap();
            socket.send(message).await.unwrap();
        });

        let mut socket = dial(&GuestEndpoint::HostSocket(path), Duration::from_secs(2))
            .await
            .unwrap();
        socket.send(Message::Text("ping".into())).await.unwrap();
        let echoed = socket.next().await.unwrap().unwrap();
        assert_eq!(echoed, Message::Text("ping".into()));
        envd.await.unwrap();
    }

    #[tokio::test]
    async fn probe_reports_an_absent_envd() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = GuestEndpoint::HostSocket(dir.path().join("missing.sock"));
        assert!(!probe(&endpoint, Duration::from_millis(200)).await);
    }
}
