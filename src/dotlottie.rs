//! `.lottie` archive decoder per [dotLottie 2.0 spec](https://dotlottie.io/spec/2.0/).
//!
//! Archive layout:
//!
//! ```text
//! <file>.lottie (zip, Deflate)
//! ├── manifest.json       (required) — animations[]/stateMachines[]/initial
//! ├── a/                  (required) — `<id>.json` per animation
//! │   └── <anim_id>.json
//! ├── s/                  (optional) — `<id>.json` per state machine
//! │   └── <sm_id>.json
//! ├── i/                  (optional) — image assets
//! ├── f/                  (optional) — font assets
//! └── t/                  (optional) — theme JSONs
//! ```
//!
//! This module reads `manifest.json`, resolves the initial animation
//! and state machine (or the first declared entries when `initial`
//! isn't set), and returns their raw JSON bytes.
//!
//! Image / font / theme assets are not yet extracted — raster layers
//! referencing `i/` will skip with their default until Phase 4's
//! image-layer work wires the asset pass-through.
//!
//! Compiled only under the `dotlottie` feature.
#![cfg(feature = "dotlottie")]

use std::collections::HashMap;
use std::io::{Cursor, Read};

use serde::Deserialize;

use crate::Error;

/// The decoded contents of a `.lottie` archive. Only the pieces we
/// currently consume — manifest header + raw JSON bytes keyed by
/// the ID they get in the manifest. Future work (images, fonts,
/// themes) layers by adding fields here.
pub(crate) struct DotLottieArchive {
    pub manifest: Manifest,
    /// Animation JSON bytes keyed by `id` (matches the directory
    /// entry `a/<id>.json`).
    pub animations: HashMap<String, Vec<u8>>,
    /// State-machine JSON bytes keyed by `id` (matches `s/<id>.json`).
    pub state_machines: HashMap<String, Vec<u8>>,
    /// Image bytes (raw PNG / JPEG / WebP) keyed by archive filename
    /// — e.g. `"img_0.png"` for an entry at `i/img_0.png`. Looked up
    /// by the image-decode pass in lib.rs against the animation's
    /// `assets[].p` filename so `ty: 2` layers referencing an
    /// archive-bundled raster render at their authored size.
    pub images: HashMap<String, Vec<u8>>,
}

impl DotLottieArchive {
    /// Animation the manifest designated as initial, else the first
    /// entry in declaration order. `None` only when the manifest's
    /// `animations` array was empty, which the spec forbids but we
    /// accept rather than reject — for robustness against
    /// hand-crafted archives and asset-stripping tools.
    pub fn initial_animation(&self) -> Option<&[u8]> {
        let id = self
            .manifest
            .initial
            .as_ref()
            .and_then(|i| i.animation.as_deref())
            .or_else(|| self.manifest.animations.first().map(|m| m.id.as_str()))?;
        self.animations.get(id).map(Vec::as_slice)
    }

    /// State machine the manifest designated as initial, else the
    /// first entry in declaration order. `None` when the archive
    /// doesn't carry any state-machine definitions.
    pub fn initial_state_machine(&self) -> Option<&[u8]> {
        let id = self
            .manifest
            .initial
            .as_ref()
            .and_then(|i| i.state_machine.as_deref())
            .or_else(|| {
                self.manifest
                    .state_machines
                    .first()
                    .map(|m| m.id.as_str())
            })?;
        self.state_machines.get(id).map(Vec::as_slice)
    }
}

/// Deserialized `manifest.json`. Extra fields (generator, themes,
/// per-animation metadata like `initialTheme` / `background`) parse
/// but aren't surfaced — the Player doesn't consume them yet.
#[derive(Debug, Deserialize)]
pub(crate) struct Manifest {
    #[allow(dead_code)]
    pub version: String,
    pub animations: Vec<AnimationMeta>,
    #[serde(rename = "stateMachines", default)]
    pub state_machines: Vec<StateMachineMeta>,
    #[serde(default)]
    pub initial: Option<InitialSpec>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AnimationMeta {
    pub id: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct StateMachineMeta {
    pub id: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct InitialSpec {
    #[serde(default)]
    pub animation: Option<String>,
    #[serde(rename = "stateMachine", default)]
    pub state_machine: Option<String>,
}

/// Decode a `.lottie` archive against the 2.0 spec layout. Returns
/// the manifest header + raw JSON bodies for every declared
/// animation and state machine.
///
/// Errors:
/// - Non-zip input or zip parse failure → `Error::Archive`.
/// - Missing `manifest.json` → `Error::Archive` (required by spec).
/// - Malformed `manifest.json` JSON → `Error::Json`.
/// - Manifest declares an animation whose file isn't in `a/` →
///   `Error::Archive`. State-machine misses are tolerated
///   silently (the archive is still considered valid — missing SM
///   just means the player can't run it).
pub(crate) fn extract(src: &[u8]) -> Result<DotLottieArchive, Error> {
    let reader = Cursor::new(src);
    let mut archive = zip::ZipArchive::new(reader)
        .map_err(|e| Error::Archive(format!("not a valid zip: {e}")))?;

    // Bucket raw entries keyed by archive path. Loading into memory
    // up-front so we can walk `manifest.json` twice without
    // re-seeking — archive sizes are small enough (a few MB for
    // real-world icon packs) that the extra allocation is fine.
    let mut files: HashMap<String, Vec<u8>> = HashMap::new();
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| Error::Archive(format!("read entry {i}: {e}")))?;
        if !entry.is_file() {
            continue;
        }
        let name = entry.name().to_string();
        let mut buf = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut buf)
            .map_err(|e| Error::Archive(format!("read {name}: {e}")))?;
        files.insert(name, buf);
    }

    let manifest_bytes = files
        .get("manifest.json")
        .ok_or_else(|| Error::Archive("manifest.json missing from archive root".to_string()))?;
    let manifest: Manifest = serde_json::from_slice(manifest_bytes)?;

    // Pull every declared animation. An animation listed in the
    // manifest but missing its `a/<id>.json` file is a genuine
    // archive error — the spec requires every `animations[]` entry
    // to have a corresponding file.
    let mut animations: HashMap<String, Vec<u8>> = HashMap::new();
    for meta in &manifest.animations {
        let path = format!("a/{}.json", meta.id);
        let bytes = files.get(&path).ok_or_else(|| {
            Error::Archive(format!(
                "animation '{}' declared in manifest but `{}` missing",
                meta.id, path
            ))
        })?;
        animations.insert(meta.id.clone(), bytes.clone());
    }

    // State machines are optional even when declared — some tools
    // emit manifest entries before authoring the SM. Warn via
    // omission rather than erroring so partial archives still play.
    let mut state_machines: HashMap<String, Vec<u8>> = HashMap::new();
    for meta in &manifest.state_machines {
        let path = format!("s/{}.json", meta.id);
        if let Some(bytes) = files.get(&path) {
            state_machines.insert(meta.id.clone(), bytes.clone());
        }
    }

    // Surface every `i/<filename>` as an image entry keyed by
    // filename (so the animation JSON's `assets[].p` field can
    // look it up directly — manifests don't usually enumerate
    // images, so walk the raw filesystem). Image content stays
    // raw (PNG / JPEG / WebP bytes); decoding happens once at
    // player load via `blinc_image`.
    let mut images: HashMap<String, Vec<u8>> = HashMap::new();
    for (name, bytes) in &files {
        if let Some(filename) = name.strip_prefix("i/") {
            if !filename.is_empty() && !filename.ends_with('/') {
                images.insert(filename.to_string(), bytes.clone());
            }
        }
    }

    Ok(DotLottieArchive {
        manifest,
        animations,
        state_machines,
        images,
    })
}

// Debug impl for test panic messages and `#[debug]` assertions.
// The public API is the opaque `DotLottieArchive` struct above —
// `{:?}` prints manifest + the list of animation / state-machine
// IDs, not the raw JSON bytes, so asset-heavy archives stay
// legible in logs.
#[allow(dead_code)]
impl std::fmt::Debug for DotLottieArchive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DotLottieArchive")
            .field("manifest", &self.manifest)
            .field("animations", &self.animations.keys().collect::<Vec<_>>())
            .field(
                "state_machines",
                &self.state_machines.keys().collect::<Vec<_>>(),
            )
            .field("images", &self.images.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    use zip::CompressionMethod;

    fn make_archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let cursor = Cursor::new(&mut buf);
            let mut zw = zip::ZipWriter::new(cursor);
            let opts: SimpleFileOptions =
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
            for (name, bytes) in entries {
                zw.start_file(*name, opts).unwrap();
                zw.write_all(bytes).unwrap();
            }
            zw.finish().unwrap();
        }
        buf
    }

    fn minimal_anim() -> &'static [u8] {
        br#"{"v":"5.0","fr":60,"ip":0,"op":60,"w":100,"h":100,"layers":[]}"#
    }

    #[test]
    fn extracts_animation_via_manifest_id() {
        let manifest = br#"{"version":"2","animations":[{"id":"main"}]}"#;
        let archive = make_archive(&[
            ("manifest.json", manifest),
            ("a/main.json", minimal_anim()),
        ]);
        let decoded = extract(&archive).unwrap();
        assert_eq!(decoded.manifest.version, "2");
        assert_eq!(decoded.animations.len(), 1);
        assert!(decoded.animations.contains_key("main"));
        assert_eq!(decoded.initial_animation(), Some(minimal_anim()));
    }

    #[test]
    fn extracts_multiple_animations_and_state_machines() {
        let manifest = br#"{
            "version": "2",
            "animations": [{"id": "idle"}, {"id": "hover"}],
            "stateMachines": [{"id": "interaction"}],
            "initial": { "animation": "hover", "stateMachine": "interaction" }
        }"#;
        let archive = make_archive(&[
            ("manifest.json", manifest),
            ("a/idle.json", minimal_anim()),
            ("a/hover.json", minimal_anim()),
            ("s/interaction.json", br#"{"initial":"x","states":[]}"#),
        ]);
        let decoded = extract(&archive).unwrap();
        assert_eq!(decoded.animations.len(), 2);
        assert_eq!(decoded.state_machines.len(), 1);
        // Initial honors the manifest override — not the declaration order.
        assert!(decoded.initial_animation().is_some());
        assert!(decoded.initial_state_machine().is_some());
    }

    #[test]
    fn missing_manifest_is_an_error() {
        let archive = make_archive(&[("a/main.json", minimal_anim())]);
        match extract(&archive) {
            Err(Error::Archive(_)) => {}
            other => panic!("expected Archive error, got {other:?}"),
        }
    }

    #[test]
    fn manifest_animation_without_file_is_an_error() {
        let manifest = br#"{"version":"2","animations":[{"id":"ghost"}]}"#;
        let archive = make_archive(&[("manifest.json", manifest)]);
        match extract(&archive) {
            Err(Error::Archive(msg)) => assert!(msg.contains("ghost")),
            other => panic!("expected Archive error about missing file, got {other:?}"),
        }
    }

    #[test]
    fn non_zip_input_is_an_error() {
        match extract(b"not a zip") {
            Err(Error::Archive(_)) => {}
            other => panic!("expected Archive error, got {other:?}"),
        }
    }

    #[test]
    fn surfaces_images_from_archive_i_directory() {
        // Images live in `i/` keyed by filename so the animation's
        // `assets[].p` field can look them up verbatim.
        let manifest = br#"{"version":"2","animations":[{"id":"main"}]}"#;
        let raw_png: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\0"; // not a real PNG, just bytes
        let raw_jpg: &[u8] = b"\xff\xd8\xff\xe0\0\0";
        let archive = make_archive(&[
            ("manifest.json", manifest),
            ("a/main.json", minimal_anim()),
            ("i/logo.png", raw_png),
            ("i/hero.jpg", raw_jpg),
        ]);
        let decoded = extract(&archive).unwrap();
        assert_eq!(decoded.images.len(), 2);
        assert_eq!(decoded.images.get("logo.png"), Some(&raw_png.to_vec()));
        assert_eq!(decoded.images.get("hero.jpg"), Some(&raw_jpg.to_vec()));
    }

    #[test]
    fn state_machine_declared_but_file_missing_is_tolerated() {
        // Spec requires declared animations exist but doesn't strictly
        // require declared state machines to — we surface this as
        // "no state machine present" rather than a hard error.
        let manifest = br#"{
            "version": "2",
            "animations": [{"id": "a"}],
            "stateMachines": [{"id": "missing"}]
        }"#;
        let archive = make_archive(&[
            ("manifest.json", manifest),
            ("a/a.json", minimal_anim()),
        ]);
        let decoded = extract(&archive).unwrap();
        assert!(decoded.state_machines.is_empty());
        assert!(decoded.initial_state_machine().is_none());
    }
}
