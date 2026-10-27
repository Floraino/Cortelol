//! Offline-testable request policy and a single-hop HTTPS transport prototype.
//! This standalone crate is deliberately not registered with the desktop app.

mod ipc;
mod policy;
mod transport;

pub use ipc::{IpcSession, MetadataResult};
pub use policy::{
    BrokerError, Budget, MAX_FRAME_BYTES, Method, PinnedRequest, RequestSpec, ResponseMeta,
    validate_redirect, validate_request,
};
pub use transport::{resolve_and_pin, send_one_hop};
