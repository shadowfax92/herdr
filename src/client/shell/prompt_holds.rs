//! Optional server draft gates, isolated by endpoint, boot and connection. The
//! client only paints these facts; draft timing and prompt delivery stay server-owned.

use std::collections::BTreeMap;

use super::*;
use crate::protocol::endpoint::EndpointPromptHolds;

/// One pane's hold state as the server projected it. Presence in the map
/// means the pane is holding agent prompts behind a human draft.
pub(super) struct PromptHold {
    pub held: usize,
}

/// A full replacement belongs to exactly one incarnation of one connection.
struct Projection {
    generation: u64,
    boot: String,
    holds: BTreeMap<String, PromptHold>,
}

/// Retains inactive endpoints' controls until their matching snapshot is shown.
#[derive(Default)]
pub(super) struct PromptHolds {
    endpoints: HashMap<ClientEndpointId, Projection>,
}

impl ClientShellState {
    pub(crate) fn set_endpoint_prompt_holds(
        &mut self,
        endpoint: &ClientEndpointId,
        generation: u64,
        projection: EndpointPromptHolds,
    ) {
        if self
            .endpoints
            .iter()
            .find(|entry| &entry.endpoint_id == endpoint)
            .and_then(|entry| entry.snapshot_generation)
            .is_some_and(|current| current > generation)
        {
            return;
        }
        self.prompt_holds.endpoints.insert(
            endpoint.clone(),
            Projection {
                generation,
                boot: projection.boot_id,
                holds: projection
                    .holds
                    .into_iter()
                    .map(|(pane, hold)| (pane, PromptHold { held: hold.held }))
                    .collect(),
            },
        );
    }

    /// Holds for the presented endpoint, keyed by public pane id.
    pub(super) fn prompt_holds(&self) -> Option<&BTreeMap<String, PromptHold>> {
        let snapshot = self.snapshot.as_deref()?;
        self.prompt_holds
            .endpoints
            .get(&self.active_endpoint_id)
            .filter(|projection| {
                projection.boot == snapshot.boot_id
                    && self
                        .active_snapshot_generation
                        .is_none_or(|generation| generation == projection.generation)
            })
            .map(|projection| &projection.holds)
    }

    /// Placeholder drawing, called once after pane flags. The stacked UX change
    /// replaces this body without changing the server projection or client store.
    pub(super) fn paint_prompt_holds(
        &self,
        frame: &mut FrameData,
        surface: &PaneSurfaceFrame,
        offset: Rect,
    ) {
        let Some(holds) = self.prompt_holds().filter(|holds| !holds.is_empty()) else {
            return;
        };
        let fg = crate::protocol::color_to_u32(self.config.palette.yellow);
        for pane in &surface.panes {
            let Some(hold) = holds.get(&pane.pane_id) else {
                continue;
            };
            if pane.rect.width < 4 || pane.rect.height == 0 || pane.inner_rect.y <= pane.rect.y {
                continue;
            }
            let text = if hold.held == 0 {
                "⏸".to_owned()
            } else {
                format!("⏸ {}", hold.held)
            };
            let width = text.chars().count() as u16;
            if width + 2 > pane.rect.width {
                continue;
            }
            let x = offset
                .x
                .saturating_add(pane.rect.x + pane.rect.width - width - 1);
            let y = offset.y.saturating_add(pane.rect.y);
            if y >= frame.height || x.saturating_add(width) > frame.width {
                continue;
            }
            for (index, symbol) in text.chars().enumerate() {
                if let Some(cell) = frame
                    .cells
                    .get_mut(y as usize * frame.width as usize + x as usize + index)
                {
                    cell.symbol = symbol.to_string();
                    cell.fg = fg;
                }
            }
        }
    }
}
