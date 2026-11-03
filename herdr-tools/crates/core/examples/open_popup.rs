//! Opens a `shadowfax.tools` popup through [`Host::open_popup`], exactly as a mode
//! would, and prints the size config supplied (`null`: the manifest default).
//!
//! ```text
//! open_popup <entrypoint> <popup_key> [KEY=VALUE …]
//! ```
//!
//! Run it with a plugin action's environment: `HERDR_SOCKET_PATH`,
//! `HERDR_PLUGIN_CONFIG_DIR` (whose `config.toml` sizes the popup) and
//! `HERDR_PLUGIN_CONTEXT_JSON` (whose focused pane is measured for profiles).
//! `tests/real_scaffold.py` drives it against an isolated Herdr.

use anyhow::{Context, Result};
use serde_json::json;
use tools_core::Host;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let entrypoint = args
        .next()
        .context("usage: open_popup <entrypoint> <popup_key> [KEY=VALUE …]")?;
    let popup_key = args.next().context("missing <popup_key>")?;
    let env = args
        .map(|pair| {
            pair.split_once('=')
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .with_context(|| format!("{pair:?} is not KEY=VALUE"))
        })
        .collect::<Result<Vec<_>>>()?;
    let env = env
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect::<Vec<_>>();
    let resolved = Host::from_env().open_popup(&entrypoint, &popup_key, &env)?;
    let report = resolved.map(|popup| {
        json!({"width": popup.dims.width, "height": popup.dims.height, "profile": popup.profile})
    });
    println!("{}", report.unwrap_or(serde_json::Value::Null));
    Ok(())
}
