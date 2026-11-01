//! A fixture's published Chrome reference: `chrome.png`, `chrome.json` and the
//! `chrome.meta.json` sidecar in `fixtures/<id>/`.
//!
//! The sidecar is the reference's commit marker. Capture writes all three into
//! a staging directory, and [`publish`] removes the published sidecar before it
//! replaces either artifact and moves the new sidecar in only after both. So a
//! sidecar that is present describes the PNG and JSON beside it, and a publish
//! interrupted anywhere leaves no sidecar, which [`inspect`] reports as
//! unusable rather than letting a later run score half of one capture against
//! half of another. That holds for brokkr as the only writer, under the
//! command lock; a hand edit of the files is not something it can detect.
//!
//! The sidecar carries the viewport the capture ran at, so a reference taken
//! at another width than the fixture now resolves to is unusable as well, and
//! a `reference_id` that brokkr assigns. Each scored result records the id of
//! the reference it was scored against, which is how `approve` tells that the
//! reference has been replaced since. capture.js writes the sidecar without an
//! id; brokkr adds one before publishing, and gives a published sidecar from
//! before ids existed one on its next run ([`adopt`]).

use std::path::{Path, PathBuf};

use crate::error::DevError;

const PNG: &str = "chrome.png";
const JSON: &str = "chrome.json";
const META: &str = "chrome.meta.json";

/// The three files of one reference generation in `dir`.
pub(crate) struct ReferenceFiles {
    pub(crate) png: PathBuf,
    pub(crate) json: PathBuf,
    pub(crate) meta: PathBuf,
}

impl ReferenceFiles {
    pub(crate) fn in_dir(dir: &Path) -> Self {
        Self {
            png: dir.join(PNG),
            json: dir.join(JSON),
            meta: dir.join(META),
        }
    }
}

/// What [`inspect`] found.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Inspection {
    /// Complete, at the requested width, identified.
    Usable { reference_id: String },
    /// Complete and at the requested width, but its sidecar predates
    /// reference ids. [`adopt`] gives it one.
    Unidentified,
    /// Not a reference at the requested width; the reason says why.
    Unusable(String),
}

/// Judge the reference in `dir` for a comparison at `width`. A missing file,
/// an unreadable or malformed sidecar, or a capture at another width is
/// [`Inspection::Unusable`]; only I/O failures other than a missing file are
/// errors.
pub(crate) fn inspect(dir: &Path, width: u32) -> Result<Inspection, DevError> {
    let files = ReferenceFiles::in_dir(dir);
    for path in [&files.png, &files.json] {
        if !path.try_exists()? {
            return Ok(Inspection::Unusable(format!(
                "{} is missing",
                file_name(path)
            )));
        }
    }
    let meta = match read_meta(&files.meta)? {
        Ok(meta) => meta,
        Err(reason) => return Ok(Inspection::Unusable(reason)),
    };
    Ok(judge_meta(&meta, width))
}

/// Give an unidentified published reference an id, rewriting its sidecar
/// atomically, and return the id. Only for a reference [`inspect`] found
/// [`Inspection::Unidentified`] at `width`.
pub(crate) fn adopt(dir: &Path, width: u32) -> Result<String, DevError> {
    let path = ReferenceFiles::in_dir(dir).meta;
    let mut meta = match read_meta(&path)? {
        Ok(meta) => meta,
        Err(reason) => return Err(DevError::Verify(reason)),
    };
    if judge_meta(&meta, width) != Inspection::Unidentified {
        return Err(DevError::Internal(format!(
            "{} is not an unidentified reference at {width}px",
            path.display()
        )));
    }
    let id = crate::db::generate_uuid()?;
    stamp_id(&mut meta, &id);
    write_meta(&path, &meta)?;
    Ok(id)
}

/// Publish the capture staged in `staging` as the reference in `dir`, after
/// checking it is complete and taken at `width`, and return its new id.
///
/// Nothing published is touched until the staged capture has passed and been
/// given its id. Then the published sidecar goes first, and a failure to
/// remove it stops before either artifact is replaced; the artifacts follow,
/// and the sidecar last.
pub(crate) fn publish(staging: &Path, dir: &Path, width: u32) -> Result<String, DevError> {
    let staged = ReferenceFiles::in_dir(staging);
    let mut meta = match inspect(staging, width)? {
        Inspection::Unidentified => match read_meta(&staged.meta)? {
            Ok(meta) => meta,
            Err(reason) => return Err(DevError::Verify(reason)),
        },
        Inspection::Usable { .. } => {
            return Err(DevError::Verify(
                "capture wrote a reference_id; brokkr assigns it".into(),
            ));
        }
        Inspection::Unusable(reason) => {
            return Err(DevError::Verify(format!(
                "capture did not produce a complete reference: {reason}"
            )));
        }
    };
    let id = crate::db::generate_uuid()?;
    stamp_id(&mut meta, &id);
    write_meta(&staged.meta, &meta)?;

    let published = ReferenceFiles::in_dir(dir);
    match std::fs::remove_file(&published.meta) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(DevError::Verify(format!(
                "cannot retire the published {}: {e}; the old reference is left as it was",
                published.meta.display()
            )));
        }
    }
    std::fs::rename(&staged.png, &published.png)?;
    std::fs::rename(&staged.json, &published.json)?;
    std::fs::rename(&staged.meta, &published.meta)?;
    Ok(id)
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// The sidecar as a JSON object, or the reason it is not one. Kept as a
/// generic object so adopting an id leaves the fields capture.js wrote as
/// they were.
fn read_meta(
    path: &Path,
) -> Result<Result<serde_json::Map<String, serde_json::Value>, String>, DevError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Err(format!("{META} is missing")));
        }
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
            return Ok(Err(format!("{META} is not UTF-8")));
        }
        Err(e) => return Err(e.into()),
    };
    Ok(match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(serde_json::Value::Object(map)) => Ok(map),
        Ok(_) => Err(format!("{META} is not a JSON object")),
        Err(e) => Err(format!("{META} is not valid JSON: {e}")),
    })
}

fn judge_meta(meta: &serde_json::Map<String, serde_json::Value>, width: u32) -> Inspection {
    let captured = meta
        .get("viewport")
        .and_then(|viewport| viewport.get("width"))
        .and_then(serde_json::Value::as_u64);
    match captured {
        None => return Inspection::Unusable(format!("{META} records no viewport width")),
        Some(captured) if captured != u64::from(width) => {
            return Inspection::Unusable(format!(
                "captured at {captured}px, the fixture renders at {width}px"
            ));
        }
        Some(_) => {}
    }
    match meta.get("reference_id") {
        None => Inspection::Unidentified,
        Some(serde_json::Value::String(id)) if !id.is_empty() => Inspection::Usable {
            reference_id: id.clone(),
        },
        Some(_) => Inspection::Unusable(format!("{META} has a malformed reference_id")),
    }
}

fn stamp_id(meta: &mut serde_json::Map<String, serde_json::Value>, id: &str) {
    meta.insert("reference_id".into(), serde_json::Value::String(id.into()));
}

fn write_meta(
    path: &Path,
    meta: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), DevError> {
    let mut text = serde_json::to_string_pretty(meta)
        .map_err(|e| DevError::Internal(format!("cannot serialize {META}: {e}")))?;
    text.push('\n');
    crate::atomic_write::replace(path, text.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;

    fn write_reference(dir: &Path, meta: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(PNG), b"png").unwrap();
        std::fs::write(dir.join(JSON), b"[]").unwrap();
        std::fs::write(dir.join(META), meta).unwrap();
    }

    const META_800: &str = r#"{"browser":"Chrome","viewport":{"width":800,"height":600}}"#;

    #[test]
    fn a_reference_is_usable_only_complete_at_the_requested_width() {
        let dir = crate::test_scratch::scratch("litehtml_reference", "inspect");
        write_reference(&dir, META_800);
        assert_eq!(inspect(&dir, 800).unwrap(), Inspection::Unidentified);
        match inspect(&dir, 720).unwrap() {
            Inspection::Unusable(reason) => assert!(reason.contains("captured at 800px")),
            other => panic!("{other:?}"),
        }

        std::fs::write(dir.join(META), "{").unwrap();
        assert!(matches!(
            inspect(&dir, 800).unwrap(),
            Inspection::Unusable(_)
        ));
        std::fs::write(dir.join(META), r#"{"viewport":{}}"#).unwrap();
        assert!(matches!(
            inspect(&dir, 800).unwrap(),
            Inspection::Unusable(_)
        ));
        std::fs::write(
            dir.join(META),
            r#"{"viewport":{"width":800},"reference_id":7}"#,
        )
        .unwrap();
        assert!(matches!(
            inspect(&dir, 800).unwrap(),
            Inspection::Unusable(_)
        ));

        std::fs::write(dir.join(META), META_800).unwrap();
        std::fs::remove_file(dir.join(PNG)).unwrap();
        assert_eq!(
            inspect(&dir, 800).unwrap(),
            Inspection::Unusable("chrome.png is missing".into())
        );
    }

    #[test]
    fn adopting_assigns_an_id_and_keeps_the_capture_fields() {
        let dir = crate::test_scratch::scratch("litehtml_reference", "adopt");
        write_reference(&dir, META_800);
        let id = adopt(&dir, 800).unwrap();
        assert_eq!(
            inspect(&dir, 800).unwrap(),
            Inspection::Usable {
                reference_id: id.clone()
            }
        );
        let text = std::fs::read_to_string(dir.join(META)).unwrap();
        assert!(text.contains("\"browser\": \"Chrome\""));
        assert!(
            adopt(&dir, 800).is_err(),
            "an identified reference is not re-adopted"
        );
    }

    #[test]
    fn publishing_replaces_the_reference_with_a_new_identity() {
        let root = crate::test_scratch::scratch("litehtml_reference", "publish");
        let dir = root.join("fixture");
        let staging = root.join("staging");
        write_reference(&dir, META_800);
        let old = adopt(&dir, 800).unwrap();

        write_reference(&staging, r#"{"viewport":{"width":720,"height":600}}"#);
        std::fs::write(staging.join(PNG), b"new png").unwrap();
        let new = publish(&staging, &dir, 720).unwrap();
        assert_ne!(old, new);
        assert_eq!(
            inspect(&dir, 720).unwrap(),
            Inspection::Usable { reference_id: new }
        );
        assert_eq!(std::fs::read(dir.join(PNG)).unwrap(), b"new png");
    }

    #[test]
    fn an_incomplete_or_wrong_width_capture_leaves_the_published_reference_alone() {
        let root = crate::test_scratch::scratch("litehtml_reference", "refuse");
        let dir = root.join("fixture");
        let staging = root.join("staging");
        write_reference(&dir, META_800);
        let old = adopt(&dir, 800).unwrap();

        write_reference(&staging, r#"{"viewport":{"width":720}}"#);
        assert!(publish(&staging, &dir, 800).is_err());
        std::fs::remove_file(staging.join(JSON)).unwrap();
        assert!(publish(&staging, &dir, 720).is_err());
        assert_eq!(
            inspect(&dir, 800).unwrap(),
            Inspection::Usable { reference_id: old }
        );
    }

    #[test]
    fn an_interrupted_publish_leaves_no_usable_reference() {
        let dir = crate::test_scratch::scratch("litehtml_reference", "interrupted");
        write_reference(&dir, META_800);
        adopt(&dir, 800).unwrap();
        // The state publish leaves between retiring the sidecar and moving the
        // new one in: artifacts of either generation, no marker.
        std::fs::remove_file(dir.join(META)).unwrap();
        assert!(matches!(
            inspect(&dir, 800).unwrap(),
            Inspection::Unusable(_)
        ));
    }
}
