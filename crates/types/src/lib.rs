//! Shared wire contracts between the dispatcher, the webhook proxy and the VM
//! entrypoint: identifiers (and THE fleet-wide runner-name derivation), the
//! run payload, webhook signature verification, and the Function-URL response
//! shape.

pub mod fnurl;
mod id;
mod idle;
mod payload;
pub mod sig;

pub use id::{MicrovmId, OurRunner, RunnerName};
pub use idle::{IdleEvent, IdleReason, IdleReport};
pub use payload::RunPayload;
