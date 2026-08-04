//! Bridge Cloud Hypervisor serial Unix sockets to gRPC console streams.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::Stream;
use tonic::Status;

use crate::path_safety;
use crate::proto;

pub type ConsoleOutboundStream =
    Pin<Box<dyn Stream<Item = Result<proto::ConsoleMessage, Status>> + Send + 'static>>;

/// Validate `vm_name` and resolve `/run/kcore/<vm>.serial.sock` (or configured dir).
#[allow(clippy::result_large_err)]
pub fn serial_socket_path(socket_dir: &Path, vm_name: &str) -> Result<PathBuf, Status> {
    let safe = path_safety::validate_safe_segment(vm_name.trim(), "vm_name")
        .map_err(Status::invalid_argument)?;
    Ok(socket_dir.join(format!("{safe}.serial.sock")))
}

/// Connect to the VM serial socket, returning typed gRPC statuses on failure.
#[allow(clippy::result_large_err)]
pub async fn connect_serial_socket(path: &Path) -> Result<UnixStream, Status> {
    match UnixStream::connect(path).await {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == ErrorKind::NotFound => Err(Status::not_found(format!(
            "serial socket not found at {}: is the VM running?",
            path.display()
        ))),
        Err(e) if e.kind() == ErrorKind::ConnectionRefused => {
            Err(Status::failed_precondition(format!(
                "serial socket refused at {}: VM may not be running",
                path.display()
            )))
        }
        Err(e) => Err(Status::unavailable(format!(
            "connect {}: {e}",
            path.display()
        ))),
    }
}

/// Bidirectional copy between a gRPC inbound stream and a Unix serial socket.
///
/// `first` is the already-read opening message (must carry `vm_name`; may carry data).
/// Remaining client messages are read from `inbound`.
#[allow(clippy::result_large_err)]
pub async fn bridge_console_session(
    socket_dir: &Path,
    first: proto::ConsoleMessage,
    mut inbound: tonic::Streaming<proto::ConsoleMessage>,
) -> Result<ConsoleOutboundStream, Status> {
    let vm_name = first.vm_name.trim().to_string();
    let path = serial_socket_path(socket_dir, &vm_name)?;
    let unix = connect_serial_socket(&path).await?;
    let (mut unix_read, mut unix_write) = unix.into_split();

    if !first.data.is_empty() {
        unix_write
            .write_all(&first.data)
            .await
            .map_err(|e| Status::aborted(format!("serial write: {e}")))?;
    }

    let (tx, rx) = mpsc::channel::<Result<proto::ConsoleMessage, Status>>(64);
    let tx_out = tx.clone();

    tokio::spawn(async move {
        let mut buf = vec![0u8; 4096];
        loop {
            match unix_read.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let msg = proto::ConsoleMessage {
                        vm_name: String::new(),
                        data: buf[..n].to_vec(),
                    };
                    if tx_out.send(Ok(msg)).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx_out
                        .send(Err(Status::aborted(format!("serial read: {e}"))))
                        .await;
                    break;
                }
            }
        }
    });

    tokio::spawn(async move {
        while let Ok(Some(msg)) = inbound.message().await {
            if !msg.vm_name.trim().is_empty() && msg.vm_name.trim() != vm_name {
                let _ = unix_write.shutdown().await;
                break;
            }
            if msg.data.is_empty() {
                continue;
            }
            if let Err(e) = unix_write.write_all(&msg.data).await {
                let _ = tx
                    .send(Err(Status::aborted(format!("serial write: {e}"))))
                    .await;
                break;
            }
        }
        let _ = unix_write.shutdown().await;
    });

    Ok(Box::pin(ReceiverStream::new(rx)) as ConsoleOutboundStream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;
    use tokio::sync::Mutex;

    #[test]
    fn serial_socket_path_rejects_traversal() {
        let dir = Path::new("/run/kcore");
        assert!(serial_socket_path(dir, "../etc").is_err());
        assert!(serial_socket_path(dir, "foo/bar").is_err());
        assert!(serial_socket_path(dir, "").is_err());
    }

    #[test]
    fn serial_socket_path_ok() {
        let dir = Path::new("/run/kcore");
        let p = serial_socket_path(dir, "web-01").unwrap();
        assert_eq!(p, PathBuf::from("/run/kcore/web-01.serial.sock"));
    }

    #[tokio::test]
    async fn connect_serial_socket_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gone.serial.sock");
        let err = connect_serial_socket(&path).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn connect_and_echo_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("echo.serial.sock");
        let listener = UnixListener::bind(&path).unwrap();

        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 16];
            let n = sock.read(&mut buf).await.unwrap();
            sock.write_all(&buf[..n]).await.unwrap();
        });

        let mut client = connect_serial_socket(&path).await.unwrap();
        client.write_all(b"hi").await.unwrap();
        let mut out = [0u8; 16];
        let n = client.read(&mut out).await.unwrap();
        assert_eq!(&out[..n], b"hi");
        server.await.unwrap();
    }

    /// Minimal stand-in: write first payload through bridge path helpers.
    #[tokio::test]
    async fn bridge_writes_first_data_and_reads_reply() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("vm1.serial.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 64];
            let n = sock.read(&mut buf).await.unwrap();
            seen2.lock().await.extend_from_slice(&buf[..n]);
            sock.write_all(b"pong").await.unwrap();
        });

        let unix = connect_serial_socket(&sock_path).await.unwrap();
        let (mut unix_read, mut unix_write) = unix.into_split();
        unix_write.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 64];
        let n = unix_read.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"pong");
        assert_eq!(seen.lock().await.as_slice(), b"ping");
        server.await.unwrap();
    }
}
