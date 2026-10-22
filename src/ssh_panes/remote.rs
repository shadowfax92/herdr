//! Remote payload entry points and the hex JSON handoff across the SSH command boundary.
//! The spec is data only: decoding never starts a process or changes a remote socket.

use std::{collections::BTreeMap, io, path::Path};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RemoteSpec {
    pub v: u32,
    pub link: String,
    pub socket: String,
    pub server_key: String,
    pub host: String,
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub cwd: Option<String>,
    pub payload_digest: String,
    pub env: BTreeMap<String, String>,
}

impl RemoteSpec {
    pub fn encode(&self) -> io::Result<String> {
        let bytes = serde_json::to_vec(self).map_err(io::Error::other)?;
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            encoded.push(HEX[(byte >> 4) as usize] as char);
            encoded.push(HEX[(byte & 15) as usize] as char);
        }
        Ok(encoded)
    }

    pub fn decode(hex: &str) -> io::Result<Self> {
        if !hex.len().is_multiple_of(2) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "remote spec must be hex-encoded JSON",
            ));
        }
        let bytes = hex
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let digit = |byte: u8| (byte as char).to_digit(16).map(|value| value as u8);
                match (digit(pair[0]), digit(pair[1])) {
                    (Some(high), Some(low)) => Ok(high * 16 + low),
                    _ => Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "remote spec contains non-hex bytes",
                    )),
                }
            })
            .collect::<io::Result<Vec<_>>>()?;
        let spec: Self = serde_json::from_slice(&bytes)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
        if spec.v != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported remote spec version",
            ));
        }
        Ok(spec)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct WatchArgs {
    pub link: String,
    pub shell_pid: u32,
    pub server_key: String,
    pub socket: String,
}

pub fn remote_attach(_spec_hex: &str) -> io::Result<i32> {
    not_implemented()
}

pub fn remote_watch(_args: WatchArgs) -> io::Result<i32> {
    not_implemented()
}

pub fn remote_run(_spec_hex: &str, _argv: &[String]) -> io::Result<i32> {
    not_implemented()
}

fn not_implemented() -> io::Result<i32> {
    eprintln!("{}", super::NOT_IMPLEMENTED_MESSAGE);
    Ok(1)
}

pub(crate) fn server_key(hostname: &str, socket: &Path) -> String {
    let mut digest = Sha256::new();
    digest.update(hostname.as_bytes());
    digest.update(b"\0");
    digest.update(socket.as_os_str().as_encoded_bytes());
    format!("{:x}", digest.finalize())[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_panes_remote_spec_round_trips_and_rejects_bad_input() {
        let spec = RemoteSpec {
            v: 1,
            link: "link".into(),
            socket: "/tmp/herdr-sp-link.sock".into(),
            server_key: "key".into(),
            host: "studio".into(),
            pane_id: "w1:p2".into(),
            workspace_id: "w1".into(),
            tab_id: "w1:t1".into(),
            cwd: Some("/remote/a ' λ".into()),
            payload_digest: "digest".into(),
            env: BTreeMap::from([("NVIM_APPNAME".into(), "herdr-sp-nvim".into())]),
        };
        let encoded = spec.encode().unwrap();
        assert_eq!(RemoteSpec::decode(&encoded).unwrap(), spec);
        assert_eq!(RemoteSpec::decode(&encoded.to_uppercase()).unwrap(), spec);
        for invalid in ["f", "xx", "ff", "7b7d"] {
            assert!(RemoteSpec::decode(invalid).is_err());
        }
        let future = RemoteSpec { v: 2, ..spec };
        assert!(RemoteSpec::decode(&future.encode().unwrap()).is_err());
        assert_ne!(
            server_key("a", Path::new("/sock")),
            server_key("b", Path::new("/sock"))
        );
    }
}
