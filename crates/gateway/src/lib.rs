//! Client connection termination (TCP + UDP, DTLS on the UDP channel), protocol framing,
//! and request routing to backing services.
//!
//! Design: docs/PROPOSAL.md ("Networking") and docs/specs/Networking_Spec.md.
//! UDP/DTLS isn't implemented yet — see #35.

pub mod envelope;
pub mod tcp;
pub mod tls;

pub use envelope::{Envelope, EnvelopeCodec};
