use std::path::{Path, PathBuf};
use std::time::Duration;

use spaghetti_common::ipc_protocol::{
    AppInfo, DaemonStatus, Destination, IPC_VERSION, PeerRef, Request, Response, RouteInfo,
    SourceInfo, SourceSpec, SystemDeviceInfo,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum SpaghettiError {
    #[error("cannot reach the spaghetti daemon at {path}: {source}")]
    Unreachable {
        path: String,
        source: std::io::Error,
    },
    #[error("spaghetti daemon connection failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("spaghetti daemon sent an unreadable response: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("spaghetti daemon refused the request: {0}")]
    Refused(String),
    #[error("spaghetti daemon answered {request} with something else")]
    Unexpected { request: &'static str },
    #[error("spaghetti daemon did not answer within {CALL_TIMEOUT:?}")]
    Timeout,
    #[error("spaghetti daemon speaks IPC version {found}; this adapter speaks {IPC_VERSION}")]
    Version { found: u32 },
}

pub type Result<T> = std::result::Result<T, SpaghettiError>;

#[derive(Debug, Clone)]
pub struct SpaghettiClient {
    socket: PathBuf,
}

impl SpaghettiClient {
    #[must_use]
    pub fn new(socket: impl AsRef<Path>) -> Self {
        Self {
            socket: socket.as_ref().to_path_buf(),
        }
    }

    pub async fn check_version(&self) -> Result<DaemonStatus> {
        let status = self.status().await?;
        if status.ipc_version != IPC_VERSION {
            return Err(SpaghettiError::Version {
                found: status.ipc_version,
            });
        }
        Ok(status)
    }

    pub async fn status(&self) -> Result<DaemonStatus> {
        match self.call(&Request::Status).await? {
            Response::Status(status) => Ok(status),
            _ => Err(SpaghettiError::Unexpected { request: "Status" }),
        }
    }

    pub async fn apps(&self) -> Result<Vec<AppInfo>> {
        match self.call(&Request::ListApps).await? {
            Response::Apps(apps) => Ok(apps),
            _ => Err(SpaghettiError::Unexpected {
                request: "ListApps",
            }),
        }
    }

    pub async fn system_devices(&self) -> Result<Vec<SystemDeviceInfo>> {
        match self.call(&Request::ListSystemDevices).await? {
            Response::SystemDevices(devices) => Ok(devices),
            _ => Err(SpaghettiError::Unexpected {
                request: "ListSystemDevices",
            }),
        }
    }

    pub async fn sources(&self) -> Result<Vec<SourceInfo>> {
        match self.call(&Request::ListSources).await? {
            Response::Sources(sources) => Ok(sources),
            _ => Err(SpaghettiError::Unexpected {
                request: "ListSources",
            }),
        }
    }

    pub async fn routes(&self) -> Result<Vec<RouteInfo>> {
        match self.call(&Request::ListRoutes).await? {
            Response::Routes(routes) => Ok(routes),
            _ => Err(SpaghettiError::Unexpected {
                request: "ListRoutes",
            }),
        }
    }

    pub async fn create_source(&self, source: &SourceSpec, tag: &str) -> Result<u32> {
        match self
            .call(&Request::CreateSource {
                source: source.clone(),
                tag: Some(tag.to_string()),
            })
            .await?
        {
            Response::SourceCreated { source_id } => Ok(source_id),
            _ => Err(SpaghettiError::Unexpected {
                request: "CreateSource",
            }),
        }
    }

    pub async fn destroy_source(&self, source_id: u32) -> Result<()> {
        match self.call(&Request::DestroySource { source_id }).await? {
            Response::Ok => Ok(()),
            _ => Err(SpaghettiError::Unexpected {
                request: "DestroySource",
            }),
        }
    }

    pub async fn set_tap_processes(&self, source_id: u32, pids: &[u32]) -> Result<()> {
        match self
            .call(&Request::SetTapProcesses {
                source_id,
                pids: pids.to_vec(),
            })
            .await?
        {
            Response::Ok => Ok(()),
            _ => Err(SpaghettiError::Unexpected {
                request: "SetTapProcesses",
            }),
        }
    }

    pub async fn destroy_route(&self, route_id: u32) -> Result<()> {
        match self.call(&Request::DestroyRoute { route_id }).await? {
            Response::Ok => Ok(()),
            _ => Err(SpaghettiError::Unexpected {
                request: "DestroyRoute",
            }),
        }
    }

    pub async fn create_route(
        &self,
        source_id: u32,
        destination: &Destination,
        volume: f32,
    ) -> Result<u32> {
        match self
            .call(&Request::CreateRoute {
                source_id,
                destination: destination.clone(),
                volume,
            })
            .await?
        {
            Response::RouteCreated { route_id } => Ok(route_id),
            _ => Err(SpaghettiError::Unexpected {
                request: "CreateRoute",
            }),
        }
    }

    pub async fn set_remote_description(
        &self,
        peer: PeerRef,
        generation: u32,
        sdp: &str,
    ) -> Result<()> {
        match self
            .call(&Request::SetRemoteDescription {
                peer,
                generation,
                sdp: sdp.to_string(),
            })
            .await?
        {
            Response::Ok => Ok(()),
            _ => Err(SpaghettiError::Unexpected {
                request: "SetRemoteDescription",
            }),
        }
    }

    async fn call(&self, request: &Request) -> Result<Response> {
        tokio::time::timeout(CALL_TIMEOUT, self.exchange(request))
            .await
            .map_err(|_| SpaghettiError::Timeout)?
    }

    async fn exchange(&self, request: &Request) -> Result<Response> {
        let mut stream = UnixStream::connect(&self.socket).await.map_err(|source| {
            SpaghettiError::Unreachable {
                path: self.socket.display().to_string(),
                source,
            }
        })?;
        let body = serde_json::to_vec(request)?;
        let len = u32::try_from(body.len()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "request too large")
        })?;
        stream.write_all(&len.to_be_bytes()).await?;
        stream.write_all(&body).await?;
        stream.flush().await?;

        let mut len = [0; 4];
        stream.read_exact(&mut len).await?;
        let len = u32::from_be_bytes(len) as usize;
        if len > MAX_RESPONSE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("response of {len} bytes"),
            )
            .into());
        }
        let mut body = vec![0; len];
        stream.read_exact(&mut body).await?;
        match serde_json::from_slice(&body)? {
            Response::Error { message } => Err(SpaghettiError::Refused(message)),
            response => Ok(response),
        }
    }
}

#[cfg(test)]
pub mod fake {
    use std::sync::atomic::{AtomicU64, Ordering};

    use serde_json::Value;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;
    use tokio::task::JoinHandle;

    use super::SpaghettiClient;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    pub fn daemon(replies: Vec<Value>) -> (SpaghettiClient, JoinHandle<Vec<Value>>) {
        let dir = std::env::temp_dir().join(format!(
            "weave-adapter-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("daemon.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let client = SpaghettiClient::new(&path);
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for reply in replies {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut len = [0; 4];
                stream.read_exact(&mut len).await.unwrap();
                let mut body = vec![0; u32::from_be_bytes(len) as usize];
                stream.read_exact(&mut body).await.unwrap();
                requests.push(serde_json::from_slice(&body).unwrap());
                let reply = serde_json::to_vec(&reply).unwrap();
                let len = u32::try_from(reply.len()).unwrap().to_be_bytes();
                stream.write_all(&len).await.unwrap();
                stream.write_all(&reply).await.unwrap();
            }
            let _ = std::fs::remove_dir_all(&dir);
            requests
        });
        (client, task)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use spaghetti_common::ipc_protocol::{FlowState, SrtMode, SrtSocket, TapMode};

    use super::*;

    #[tokio::test]
    async fn each_call_sends_one_request_and_reads_its_answer() {
        let (client, daemon) = fake::daemon(vec![
            json!({ "SourceCreated": { "source_id": 4 } }),
            json!({ "RouteCreated": { "route_id": 9 } }),
            json!({ "Sources": [{
                "source_id": 4,
                "spec": { "Tap": { "pids": [731], "mode": "Mirror" } },
                "tag": "weave:hop",
                "label": "Music",
                "status": { "state": "Flowing", "bytes": 64, "peer": null, "relayed": false, "packets_lost": 0, "packets_retransmitted": 0 },
                "local_description": null
            }] }),
            json!({ "Error": { "message": "no such route" } }),
        ]);

        let tap = SourceSpec::Tap {
            pids: vec![731],
            mode: TapMode::Mirror,
        };
        assert_eq!(client.create_source(&tap, "weave:hop").await.unwrap(), 4);
        let destination = Destination::Srt(SrtSocket {
            mode: SrtMode::Call {
                host: "10.0.0.2".into(),
                port: 23000,
            },
            latency_ms: 200,
            passphrase: Some("0123456789".into()),
            key_size: Some(32),
        });
        assert_eq!(client.create_route(4, &destination, 1.0).await.unwrap(), 9);
        let sources = client.sources().await.unwrap();
        assert_eq!(sources[0].spec, tap);
        assert_eq!(sources[0].status.state, FlowState::Flowing);
        assert!(matches!(
            client.destroy_route(9).await,
            Err(SpaghettiError::Refused(message)) if message == "no such route"
        ));

        let seen = daemon.await.unwrap();
        assert_eq!(
            seen[0],
            json!({ "CreateSource": { "source": { "Tap": { "pids": [731], "mode": "Mirror" } }, "tag": "weave:hop" } })
        );
        assert_eq!(
            seen[1]["CreateRoute"]["destination"]["Srt"]["mode"],
            json!({ "Call": { "host": "10.0.0.2", "port": 23000 } })
        );
        assert_eq!(seen[2], json!("ListSources"));
        assert_eq!(seen[3], json!({ "DestroyRoute": { "route_id": 9 } }));
    }
}
