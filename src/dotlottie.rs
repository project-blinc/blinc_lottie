//! `.lottie` archive decoder.
//!
//! dotLottie is a zip archive standardized by the LottieFiles community
//! for distributing Lottie animations with bundled assets. Layout (from
//! the [dotLottie 2.0 spec](https://dotlottie.io/spec/2.0/)):
//!
//! ```text
//! <archive>.lottie
//! ├── manifest.json          (optional — describes bundled animations)
//! ├── animations/            (multi-animation case)
//! │   ├── anim_1.json
//! │   └── anim_2.json
//! ├── <name>.json            (single-animation case, at archive root)
//! ├── state_machine.json     (optional — state machine spec)
//! ├── state_machines/        (multi-SM case)
//! │   └── sm_1.json
//! └── images/                (optional — raster assets)
//!     └── img_0.png
//! ```
//!
//! This module stays deliberately minimal: no manifest parsing, no
//! image extraction, no multi-animation support. Returns the first
//! JSON file that looks like a Lottie animation plus the first
//! state-machine JSON if one is present. Covers the common case
//! (single-animation archive) and leaves the richer variants for
//! follow-ups that land alongside Phase 4's image-layer work.
//!
//! Compiled only when the `dotlottie` feature is enabled; consumers
//! that ship plain JSON Lotties don't pull in the `zip` crate.
#![cfg(feature = "dotlottie")]

use std::io::Cursor;
use std::io::Read;

use crate::Error;

/// Decode a `.lottie` archive and return `(animation_json,
/// state_machine_json)`. State machine JSON is `None` when the
/// archive doesn't include one.
///
/// Behaviour on malformed / missing entries:
/// - zip parse failure → `Error::Archive`
/// - no animation JSON → `Error::Archive`
/// - present but unreadable state_machine → treated as `None` and
///   parsing continues (state machine is optional; we don't want
///   a malformed SM to block the main animation)
pub(crate) fn extract_archive(src: &[u8]) -> Result<(Vec<u8>, Option<Vec<u8>>), Error> {
    let reader = Cursor::new(src);
    let mut archive = zip::ZipArchive::new(reader)
        .map_err(|e| Error::Archive(format!("not a valid zip: {e}")))?;

    let mut animation: Option<Vec<u8>> = None;
    let mut state_machine: Option<Vec<u8>> = None;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| Error::Archive(format!("read entry {i}: {e}")))?;
        if !entry.is_file() {
            continue;
        }
        let name = entry.name().to_string();
        let lower = name.to_ascii_lowercase();
        // State-machine JSON takes priority when the filename
        // matches, otherwise anything ending in .json inside the
        // state_machines/ directory. First hit wins — matches the
        // single-SM convention.
        let is_state_machine = lower.ends_with("state_machine.json")
            || lower.starts_with("state_machines/")
            || lower.contains("/state_machines/");
        if is_state_machine && state_machine.is_none() {
            let mut buf = Vec::with_capacity(entry.size() as usize);
            entry
                .read_to_end(&mut buf)
                .map_err(|e| Error::Archive(format!("read state_machine: {e}")))?;
            state_machine = Some(buf);
            continue;
        }
        // Accept `animation.json` at root, or any `.json` in
        // `animations/`, or a bare `<name>.json` at archive root.
        // Skip manifest and state_machine, which we handle above.
        let is_animation = (lower.ends_with(".json")
            && !lower.ends_with("manifest.json")
            && !is_state_machine)
            && (!lower.contains('/') || lower.starts_with("animations/"));
        if is_animation && animation.is_none() {
            let mut buf = Vec::with_capacity(entry.size() as usize);
            entry
                .read_to_end(&mut buf)
                .map_err(|e| Error::Archive(format!("read animation: {e}")))?;
            animation = Some(buf);
        }
    }

    match animation {
        Some(bytes) => Ok((bytes, state_machine)),
        None => Err(Error::Archive(
            "no animation JSON found in archive".to_string(),
        )),
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

    #[test]
    fn extracts_single_root_json() {
        let archive = make_archive(&[("animation.json", b"{\"v\":\"5.0\"}")]);
        let (anim, sm) = extract_archive(&archive).unwrap();
        assert_eq!(anim, b"{\"v\":\"5.0\"}");
        assert!(sm.is_none());
    }

    #[test]
    fn extracts_animation_plus_state_machine() {
        let archive = make_archive(&[
            ("animation.json", b"{\"v\":\"5.0\"}"),
            ("state_machine.json", b"{\"states\":[]}"),
        ]);
        let (anim, sm) = extract_archive(&archive).unwrap();
        assert_eq!(anim, b"{\"v\":\"5.0\"}");
        assert_eq!(sm.as_deref(), Some(b"{\"states\":[]}" as &[u8]));
    }

    #[test]
    fn picks_animation_from_animations_directory() {
        let archive = make_archive(&[
            ("manifest.json", b"{}"),
            ("animations/main.json", b"{\"v\":\"5.0\"}"),
        ]);
        let (anim, _) = extract_archive(&archive).unwrap();
        assert_eq!(anim, b"{\"v\":\"5.0\"}");
    }

    #[test]
    fn missing_animation_surfaces_archive_error() {
        let archive = make_archive(&[("manifest.json", b"{}")]);
        let err = extract_archive(&archive).unwrap_err();
        assert!(matches!(err, Error::Archive(_)));
    }

    #[test]
    fn non_zip_input_surfaces_archive_error() {
        let err = extract_archive(b"not a zip").unwrap_err();
        assert!(matches!(err, Error::Archive(_)));
    }
}
