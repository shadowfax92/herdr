//! Optional SSH inheritance at pane/tab creation. Returning None delegates to the
//! stock local launch path; this skeleton always takes that path and spawns nothing.

use std::collections::HashMap;

use crate::{api::schema::PaneSplitParams, app::App, layout::PaneId};

impl App {
    // Match the upstream split context without moving geometry or env policy into the registry.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn ssh_panes_split(
        &mut self,
        _id: &str,
        _ws_idx: usize,
        _target: PaneId,
        _params: &PaneSplitParams,
        _rows: u16,
        _cols: u16,
    ) -> Option<String> {
        None
    }

    // Keep the hook's inputs borrowed so None preserves the original launch values.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn ssh_panes_tab_create(
        &mut self,
        _id: &str,
        _ws_idx: usize,
        _cwd: Option<&str>,
        _env: &HashMap<String, String>,
        _focus: bool,
        _label: Option<&str>,
    ) -> Option<String> {
        None
    }
}
