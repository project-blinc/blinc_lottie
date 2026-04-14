//! Lottie JSON schema — minimal slice needed for playback.
//!
//! The full Lottie spec covers ~60 layer / shape / animation types. This
//! module starts with just the header fields and layer array as opaque
//! JSON values; richer parsing (keyframes, shape geometry, transforms)
//! is added incrementally as rendering paths grow.
//!
//! Field names follow the Lottie spec's short aliases (`v`, `fr`, `ip`,
//! `op`, `w`, `h`, `nm`, `layers`). See
//! <https://lottiefiles.github.io/lottie-docs/> for the full schema.

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub(crate) struct LottieRoot {
    /// Animation schema version. Informational.
    #[serde(rename = "v", default)]
    #[allow(dead_code)]
    pub version: Option<String>,

    /// Frames per second.
    #[serde(rename = "fr")]
    pub frame_rate: f32,

    /// In-point: first visible frame.
    #[serde(rename = "ip")]
    pub in_point: f32,

    /// Out-point: last visible frame (exclusive).
    #[serde(rename = "op")]
    pub out_point: f32,

    /// Canvas width in pixels.
    #[serde(rename = "w")]
    pub width: u32,

    /// Canvas height in pixels.
    #[serde(rename = "h")]
    pub height: u32,

    /// Composition name.
    #[serde(rename = "nm", default)]
    #[allow(dead_code)]
    pub name: Option<String>,

    /// Layer stack. Opaque for now — each entry preserves the original
    /// JSON object so richer parsing can be added without reshuffling
    /// the outer types.
    #[serde(default)]
    pub layers: Vec<serde_json::Value>,
}
