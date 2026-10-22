//! Payload digest, sync, and diagnostics boundary. The skeleton performs no file or SSH work.

use std::io;

use super::config::SshPanesConfig;

#[derive(Debug, Clone, Default)]
pub(crate) struct Manifest {
    pub digest: String,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SyncReport {
    pub changed: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct DoctorReport {
    pub diagnostics: Vec<String>,
}

pub fn local_manifest(_cfg: &SshPanesConfig) -> io::Result<Manifest> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        super::NOT_IMPLEMENTED_MESSAGE,
    ))
}

pub fn ensure_synced(_host: &str, _cfg: &SshPanesConfig, _force: bool) -> io::Result<SyncReport> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        super::NOT_IMPLEMENTED_MESSAGE,
    ))
}

pub fn doctor(_host: &str, _cfg: &SshPanesConfig) -> io::Result<DoctorReport> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        super::NOT_IMPLEMENTED_MESSAGE,
    ))
}
