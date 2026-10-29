use serde::{Deserialize, Serialize};

/// `[ui] landing_glow`: the short color pulse a pane's border gives when a jump
/// lands on it, so the user can find the landed pane among its neighbors.
///
/// Presentation only. The client draws it over the server-rendered surface;
/// nothing about it reaches the server or the API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LandingGlowConfig {
    /// A one-cell ring that blooms and settles back into the focus border.
    #[default]
    Ring,
    /// Border cells only, fainter and shorter.
    Quiet,
    /// Reduced motion: one static highlight that clears in a single step.
    Static,
    /// No glow.
    Off,
}
