//! Transport layer for inter-agent communication
//!
//! Currently supports Unix sockets with peercred authentication.

mod message;
mod unix;

pub use message::{AgentMessage, MessageKind};
pub use unix::{AgentListener, AgentConnection, PeerCredentials};
