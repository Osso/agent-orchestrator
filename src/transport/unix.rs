//! Unix socket transport with peercred authentication

use anyhow::{Context, Result};
use nix::sys::socket::{getsockopt, sockopt::PeerCredentials as NixPeerCred};
use std::path::{Path, PathBuf};
use tokio::net::{UnixListener, UnixStream};

use super::message::{wire, AgentMessage};
use crate::types::AgentRole;

/// Peer credentials from SO_PEERCRED
#[derive(Debug, Clone)]
pub struct PeerCredentials {
    pub pid: i32,
    pub uid: u32,
    pub gid: u32,
}

impl PeerCredentials {
    /// Get peer credentials from a Unix stream
    pub fn from_stream(stream: &UnixStream) -> Result<Self> {
        let cred = getsockopt(stream, NixPeerCred).context("Failed to get peer credentials")?;
        Ok(Self {
            pid: cred.pid(),
            uid: cred.uid(),
            gid: cred.gid(),
        })
    }

    /// Check if peer is running as the same user
    pub fn is_same_user(&self) -> bool {
        self.uid == nix::unistd::getuid().as_raw()
    }
}

/// Listener for incoming agent connections
pub struct AgentListener {
    listener: UnixListener,
    socket_path: PathBuf,
    role: AgentRole,
}

impl AgentListener {
    /// Create a new listener for an agent role
    ///
    /// Socket path will be: `{base_path}/{role}.sock`
    pub async fn bind(role: AgentRole, base_path: &Path) -> Result<Self> {
        let socket_path = base_path.join(format!("{}.sock", role.as_str()));

        // Remove existing socket file if present
        if socket_path.exists() {
            std::fs::remove_file(&socket_path).context("Failed to remove existing socket")?;
        }

        // Ensure parent directory exists
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent).context("Failed to create socket directory")?;
        }

        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("Failed to bind to {}", socket_path.display()))?;

        tracing::info!("Agent {} listening on {}", role, socket_path.display());

        Ok(Self {
            listener,
            socket_path,
            role,
        })
    }

    /// Accept a new connection
    ///
    /// Returns the connection and peer credentials.
    pub async fn accept(&self) -> Result<(AgentConnection, PeerCredentials)> {
        let (stream, _addr) = self.listener.accept().await.context("Failed to accept connection")?;
        let creds = PeerCredentials::from_stream(&stream)?;

        // Verify peer is same user
        if !creds.is_same_user() {
            anyhow::bail!(
                "Connection from different user: uid={}, expected={}",
                creds.uid,
                nix::unistd::getuid().as_raw()
            );
        }

        tracing::debug!(
            "Agent {} accepted connection from pid={} uid={}",
            self.role,
            creds.pid,
            creds.uid
        );

        Ok((AgentConnection::new(stream), creds))
    }

    /// Get the socket path
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

impl Drop for AgentListener {
    fn drop(&mut self) {
        // Clean up socket file
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// Connection to/from an agent
pub struct AgentConnection {
    stream: UnixStream,
}

impl AgentConnection {
    fn new(stream: UnixStream) -> Self {
        Self { stream }
    }

    /// Connect to an agent's socket
    pub async fn connect(role: AgentRole, base_path: &Path) -> Result<Self> {
        let socket_path = base_path.join(format!("{}.sock", role.as_str()));

        let stream = UnixStream::connect(&socket_path)
            .await
            .with_context(|| format!("Failed to connect to {}", socket_path.display()))?;

        Ok(Self::new(stream))
    }

    /// Send a message to the peer
    pub async fn send(&mut self, msg: &AgentMessage) -> Result<()> {
        wire::write_message(&mut self.stream, msg).await
    }

    /// Receive a message from the peer
    pub async fn recv(&mut self) -> Result<AgentMessage> {
        wire::read_message(&mut self.stream).await
    }

    /// Get peer credentials
    pub fn peer_credentials(&self) -> Result<PeerCredentials> {
        PeerCredentials::from_stream(&self.stream)
    }
}
