//! In-process SMB read transport for the Synology FileStation client.
//!
//! Reads NAS file bytes over SMB3 directly from the process — pure Rust, no OS
//! mount, no privileges, cross-platform — so bulk staging bypasses the
//! FileStation HTTP Download API and the shared `synoscgi` backend that a
//! task-per-file download fan-out can saturate.
//!
//! Validated against the AD-joined, SMB3-only `e4e-nas` (see the design notes
//! and the `spikes/smb-spike` feasibility spike). This crate is the **transport
//! only**; preferring it over HTTP with a health-based fallback is a separate
//! selection layer.
//!
//! ```no_run
//! # async fn demo() -> Result<(), synology_filestation_core::SynoFsError> {
//! use synology_filestation_smb::{SmbConfig, SmbTransport};
//!
//! let mut cfg = SmbConfig::new("e4e-nas.ucsd.edu", "c.crutchfield.642", "•••");
//! cfg.domain = "KRG".into(); // AD account
//! let smb = SmbTransport::connect(&cfg).await?;
//!
//! let meta = smb.stat("/fishsense_data/REEF/x.orf").await?;
//! let bytes = smb.read("/fishsense_data/REEF/x.orf", 0, meta.size).await?;
//! # let _ = bytes;
//! # Ok(())
//! # }
//! ```

pub mod error;
pub mod framing;
mod handles;
pub mod path;
pub mod probe;
pub mod transport;

pub use error::to_syno_error;
pub use framing::StreamTransport;
pub use path::SmbPath;
pub use probe::{probe_as, Fallback, FallbackKind, Probe};
pub use transport::{
    attach, auto_attach, auto_attach_as, auto_connect, auto_connect_as, BoxedStream, FileMeta,
    RedialFuture, SmbConfig, SmbStream, SmbTransport,
};
