// Defines Rust configuration and serialization logic for the media service.
// Author: Thomas Klute

//! Unified AI model registry reader.
//!
//! Mirrors the Python loader in `apps/model_registry.py`. Reads sidecar
//! JSON files from `config/models/*.json` and returns resolved model
//! definitions that the pipeline builder can use directly.
//!
//! Schema (see `apps/model_registry.py` for the canonical description):
//! ```json
//! {
//!   "display_name": "YOLOv8m COCO (Hailo-10H)",
//!   "scope": "object_detection",
//!   "active": true,
//!   "input": {"width": 640, "height": 640, "format": "RGB"},
//!   "hef_path": "/path/to/model.hef",
//!   "postprocess": {
//!     "so_path": "/path/to/lib.so",
//!     "function_name": "yolov8m",
//!     "output_format": "yolov8"
//!   },
//!   "labels": "coco_80",
//!   "notes": "..."
//! }
//! ```
//!
//! Loader rules: skip files that fail to parse, skip `active=false`,
//! skip models whose `hef_path` does not exist on disk, reject duplicate
//! `display_name` within a scope.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, error, warn};

pub const DEFAULT_MODELS_DIR: &str = "config/models";

// Per-family defaults for postprocess.so_path and
// postprocess.function_name. A model JSON can omit those two fields and
// the loader will fill them in from this table based on the required
// output_format. Families listed here must mirror the Python table in
// ``apps/model_registry.py``. Families absent from the table
// (``custom`` / ``centerpose``) must spell both fields out explicitly
// in the JSON or they will fail validation.
//
// ``filter_letterbox`` is the generic TAPPAS entry point that regex-
// matches any ``nms_postprocess`` tensor. It works for every NMS-baked
// YOLO HEF regardless of the exact tensor name.
const TAPPAS_YOLO_SO: &str =
    "/usr/lib/aarch64-linux-gnu/hailo/tappas/post_processes/libyolo_hailortpp_post.so";
const INTREE_YOLO26_SO: &str = "/opt/robocup-ai-camera/apps/hailo_postprocess/libyolo26_post.so";

/// Return ``Some((so_path, function_name))`` if the given output_format
/// has family defaults, ``None`` otherwise.
fn family_defaults(output_format: &str) -> Option<(&'static str, &'static str)> {
    match output_format {
        "yolov5" | "yolov8" | "yolox" => Some((TAPPAS_YOLO_SO, "filter_letterbox")),
        "yolo26" => Some((INTREE_YOLO26_SO, "yolo26")),
        // "custom" has no defaults - the JSON must spell out so_path
        // and function_name explicitly.
        _ => None,
    }
}

/// Mutate the raw JSON value to fill in ``postprocess.so_path``
/// and ``postprocess.function_name`` from ``family_defaults`` when they
/// are absent. Runs BEFORE serde deserialization so ``ModelPostprocess``
/// can keep its strict ``String`` fields.
///
/// No-op if the ``postprocess`` / ``output_format`` keys are missing or
/// malformed - serde will complain about those with a clearer error.
fn apply_family_defaults(value: &mut Value, filename: &str) {
    let Some(pp) = value.get_mut("postprocess").and_then(Value::as_object_mut) else {
        return;
    };
    let Some(fmt) = pp.get("output_format").and_then(Value::as_str) else {
        return;
    };
    let Some((default_so, default_fn)) = family_defaults(fmt) else {
        return;
    };
    let fmt_owned = fmt.to_string();
    if !pp.contains_key("so_path") {
        pp.insert("so_path".to_string(), Value::String(default_so.to_string()));
        debug!(
            file = filename,
            family = %fmt_owned,
            "model_registry: filled in postprocess.so_path from family default"
        );
    }
    if !pp.contains_key("function_name") {
        pp.insert(
            "function_name".to_string(),
            Value::String(default_fn.to_string()),
        );
        debug!(
            file = filename,
            family = %fmt_owned,
            "model_registry: filled in postprocess.function_name from family default"
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelScope {
    ObjectDetection,
}

impl ModelScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            ModelScope::ObjectDetection => "object_detection",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelInputSpec {
    pub width: u32,
    pub height: u32,
    pub format: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ModelPostprocess {
    #[serde(default)]
    pub so_path: String,
    #[serde(default)]
    pub function_name: String,
    #[serde(default)]
    pub output_format: String,
}

/// The `labels` field in a model sidecar JSON can be either
/// a **named set string** (`"coco_80"` - legacy UI hint, not consumed
/// by the pipeline) or an **index map array** (`["red", "blue", ...]`
/// - `labels[class_index]` gives the human-readable name). The
/// `#[serde(untagged)]` attribute tries each variant in order.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ModelLabels {
    /// Legacy named-set hint (e.g. `"coco_80"`). UI-facing metadata.
    Named(String),
    /// Actual index→name mapping. `labels[i]` is the class name for
    /// output index `i`. Consumed by the pipeline to override
    /// whatever the postprocess `.so` produces.
    IndexMap(Vec<String>),
}

impl ModelLabels {
    /// Return the index map if available, `None` for a named set.
    pub fn as_index_map(&self) -> Option<&[String]> {
        match self {
            ModelLabels::IndexMap(v) => Some(v.as_slice()),
            ModelLabels::Named(_) => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelDef {
    pub display_name: String,
    pub scope: ModelScope,
    pub active: bool,
    pub input: ModelInputSpec,
    /// Runtime: "hailo" (default) or "pytorch" / "onnx" for CPU models.
    #[serde(default = "default_runtime")]
    pub runtime: String,
    /// Path to .hef file (Hailo models).
    #[serde(default)]
    pub hef_path: Option<String>,
    /// Path to .pt / .onnx file (CPU models).
    #[serde(default)]
    pub model_path: Option<String>,
    #[serde(default)]
    pub postprocess: ModelPostprocess,
    #[serde(default)]
    pub labels: Option<ModelLabels>,
    /// Optional integer class-ID → pipeline label remapping.
    /// Keys are source class IDs (as strings in JSON), values are
    /// target labels ("ball", "robot", "human"). Unlisted IDs are
    /// dropped. When absent, the existing substring-based
    /// `map_class_label()` in metadata_export is used instead.
    #[serde(default)]
    pub class_map: Option<std::collections::HashMap<String, String>>,
    /// Target inference rate in frames per second. When set, the pipeline
    /// inserts a `videorate` element before inference to drop frames.
    /// Also controls frame_export `subsample` for Python consumers.
    /// Default (None) → 3 fps.
    #[serde(default)]
    pub inference_fps: Option<f32>,
    #[serde(default)]
    pub notes: Option<String>,
    /// When `false`, the pipeline skips the `meta_export` hailofilter
    /// for this model. Render-only models (e.g. pose estimation) emit
    /// HailoLandmarks the meta_export filter has no business publishing
    /// on the `object_detections` ZMQ topic.
    #[serde(default = "default_publish_detections")]
    pub publish_detections: bool,
}

fn default_publish_detections() -> bool {
    true
}

fn default_runtime() -> String {
    "hailo".to_string()
}

fn parse_one(path: &Path) -> Option<ModelDef> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            warn!(
                file = %path.display(),
                error = %e,
                "model_registry: failed to read file"
            );
            return None;
        }
    };
    // Parse as a generic Value first so we can apply family
    // defaults for postprocess.so_path / function_name before running
    // the strict ModelDef deserialization.
    let mut value: Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            warn!(
                file = %path.display(),
                error = %e,
                "model_registry: invalid JSON"
            );
            return None;
        }
    };
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("<unknown>");
    apply_family_defaults(&mut value, filename);
    match serde_json::from_value::<ModelDef>(value) {
        Ok(md) => Some(md),
        Err(e) => {
            warn!(
                file = %path.display(),
                error = %e,
                "model_registry: schema validation failed"
            );
            None
        }
    }
}

/// Load all models in the registry, filtered by scope if given.
///
/// Applies the same filtering rules as the Python loader:
/// - parse failures → skipped with warning
/// - `active=false` → hidden
/// - missing `hef_path` file → hidden with warning
/// - duplicate `display_name` within a scope → both dropped with error
pub fn load_models(directory: &Path, scope: Option<ModelScope>) -> Vec<ModelDef> {
    if !directory.is_dir() {
        warn!(
            dir = %directory.display(),
            "model_registry: registry directory not found"
        );
        return Vec::new();
    }

    // Collect all parsed files (sorted for determinism).
    let mut entries: Vec<PathBuf> = match std::fs::read_dir(directory) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect(),
        Err(e) => {
            warn!(
                dir = %directory.display(),
                error = %e,
                "model_registry: failed to read directory"
            );
            return Vec::new();
        }
    };
    entries.sort();

    let mut parsed: Vec<(PathBuf, ModelDef)> = Vec::new();
    for path in entries {
        if let Some(md) = parse_one(&path) {
            parsed.push((path, md));
        }
    }

    // Reject duplicate display_name within scope.
    let mut seen: HashMap<(ModelScope, String), PathBuf> = HashMap::new();
    let mut duplicates: HashSet<(ModelScope, String)> = HashSet::new();
    for (src, md) in &parsed {
        let key = (md.scope, md.display_name.clone());
        if let Some(prev) = seen.get(&key) {
            error!(
                display_name = %md.display_name,
                scope = md.scope.as_str(),
                file_a = %prev.display(),
                file_b = %src.display(),
                "model_registry: duplicate display_name, both files dropped"
            );
            duplicates.insert(key);
        } else {
            seen.insert(key, src.clone());
        }
    }

    let mut results: Vec<ModelDef> = Vec::new();
    for (src, md) in parsed {
        let key = (md.scope, md.display_name.clone());
        if duplicates.contains(&key) {
            continue;
        }
        if !md.active {
            continue;
        }
        if let Some(req) = scope {
            if md.scope != req {
                continue;
            }
        }
        // Validate model file based on runtime
        if md.runtime == "hailo" {
            if let Some(ref hef) = md.hef_path {
                if !Path::new(hef).exists() {
                    warn!(
                        file = %src.display(),
                        hef_path = %hef,
                        "model_registry: hef_path does not exist, model hidden"
                    );
                    continue;
                }
            } else {
                warn!(file = %src.display(), "model_registry: hailo model missing hef_path");
                continue;
            }
            // Hailo models must have postprocess configured
            if md.postprocess.so_path.is_empty() || md.postprocess.function_name.is_empty() {
                warn!(file = %src.display(), "model_registry: hailo model missing postprocess config");
                continue;
            }
            if !Path::new(&md.postprocess.so_path).exists() {
                warn!(
                    file = %src.display(),
                    so_path = %md.postprocess.so_path,
                    "model_registry: postprocess.so_path does not exist, model hidden"
                );
                continue;
            }
        } else {
            // pytorch / onnx: validate model_path
            if let Some(ref mp) = md.model_path {
                if !Path::new(mp).exists() {
                    warn!(
                        file = %src.display(),
                        model_path = %mp,
                        "model_registry: model_path does not exist, model hidden"
                    );
                    continue;
                }
            } else {
                warn!(file = %src.display(), "model_registry: CPU model missing model_path");
                continue;
            }
        }
        results.push(md);
    }
    results.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    results
}

/// Look up a single model by display name + (optional) scope.
///
/// Returns `None` if no match - which includes the model being absent,
/// inactive, missing its hef, or dropped due to a duplicate name.
pub fn load_model_by_display_name(
    directory: &Path,
    display_name: &str,
    scope: Option<ModelScope>,
) -> Option<ModelDef> {
    load_models(directory, scope)
        .into_iter()
        .find(|m| m.display_name == display_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write_model(
        dir: &Path,
        filename: &str,
        display_name: &str,
        scope: &str,
        active: bool,
        hef_path: Option<&str>,
    ) {
        write_model_full(dir, filename, display_name, scope, active, hef_path, None);
    }

    /// Like `write_model` but also lets the caller override the
    /// postprocess.so_path. Default: a fresh fake .so beside the model
    /// JSON so both existence checks pass.
    fn write_model_full(
        dir: &Path,
        filename: &str,
        display_name: &str,
        scope: &str,
        active: bool,
        hef_path: Option<&str>,
        so_path: Option<&str>,
    ) {
        let hef = match hef_path {
            Some(p) => p.to_string(),
            None => {
                let p = dir.join(format!("{}.hef", filename));
                fs::write(&p, b"\x00").unwrap();
                p.to_string_lossy().to_string()
            }
        };
        let so = match so_path {
            Some(p) => p.to_string(),
            None => {
                let p = dir.join(format!("{}.so", filename));
                fs::write(&p, b"\x00").unwrap();
                p.to_string_lossy().to_string()
            }
        };
        let body = format!(
            r#"{{
                "display_name": "{name}",
                "scope": "{scope}",
                "active": {active},
                "input": {{"width": 640, "height": 640, "format": "RGB"}},
                "hef_path": "{hef}",
                "postprocess": {{
                    "so_path": "{so}",
                    "function_name": "filter_letterbox",
                    "output_format": "yolov8"
                }}
            }}"#,
            name = display_name,
            scope = scope,
            active = active,
            hef = hef,
            so = so,
        );
        fs::write(dir.join(filename), body).unwrap();
    }

    #[test]
    fn missing_dir_returns_empty() {
        let tmp = tempdir().unwrap();
        let models = load_models(&tmp.path().join("nope"), None);
        assert!(models.is_empty());
    }

    #[test]
    fn active_false_is_hidden() {
        let tmp = tempdir().unwrap();
        write_model(tmp.path(), "on.json", "On", "object_detection", true, None);
        write_model(
            tmp.path(),
            "off.json",
            "Off",
            "object_detection",
            false,
            None,
        );
        let models = load_models(tmp.path(), None);
        let names: Vec<_> = models.iter().map(|m| m.display_name.as_str()).collect();
        assert_eq!(names, vec!["On"]);
    }

    #[test]
    fn missing_hef_is_hidden() {
        let tmp = tempdir().unwrap();
        write_model(
            tmp.path(),
            "good.json",
            "Good",
            "object_detection",
            true,
            None,
        );
        write_model(
            tmp.path(),
            "bad.json",
            "Bad",
            "object_detection",
            true,
            Some("/nonexistent/no.hef"),
        );
        let models = load_models(tmp.path(), None);
        let names: Vec<_> = models.iter().map(|m| m.display_name.as_str()).collect();
        assert_eq!(names, vec!["Good"]);
    }

    #[test]
    fn missing_postprocess_so_is_hidden() {
        // Same safety net on the Rust side: if a model's .so doesn't
        // exist, resolve_ai_config must not return it - otherwise
        // hailofilter segfaults at pipeline build time.
        let tmp = tempdir().unwrap();
        write_model(
            tmp.path(),
            "good.json",
            "Good",
            "object_detection",
            true,
            None,
        );
        write_model_full(
            tmp.path(),
            "bad.json",
            "Bad",
            "object_detection",
            true,
            None,
            Some("/nonexistent/libno.so"),
        );
        let models = load_models(tmp.path(), None);
        let names: Vec<_> = models.iter().map(|m| m.display_name.as_str()).collect();
        assert_eq!(names, vec!["Good"]);
    }

    #[test]
    fn scope_filter_works() {
        let tmp = tempdir().unwrap();
        write_model(tmp.path(), "od.json", "OD", "object_detection", true, None);
        let od = load_models(tmp.path(), Some(ModelScope::ObjectDetection));
        assert_eq!(od.len(), 1);
        assert_eq!(od[0].display_name, "OD");
        // No filter → same single model.
        assert_eq!(load_models(tmp.path(), None).len(), 1);
    }

    #[test]
    fn object_detection_scope_as_str() {
        assert_eq!(ModelScope::ObjectDetection.as_str(), "object_detection");
    }

    #[test]
    fn duplicate_display_name_drops_both() {
        let tmp = tempdir().unwrap();
        // Use distinct hef files for each so existence check passes.
        let h1 = tmp.path().join("h1.hef");
        let h2 = tmp.path().join("h2.hef");
        let h3 = tmp.path().join("h3.hef");
        fs::write(&h1, b"\x00").unwrap();
        fs::write(&h2, b"\x00").unwrap();
        fs::write(&h3, b"\x00").unwrap();
        write_model(
            tmp.path(),
            "a.json",
            "Duplicated",
            "object_detection",
            true,
            Some(h1.to_str().unwrap()),
        );
        write_model(
            tmp.path(),
            "b.json",
            "Duplicated",
            "object_detection",
            true,
            Some(h2.to_str().unwrap()),
        );
        write_model(
            tmp.path(),
            "c.json",
            "Unique",
            "object_detection",
            true,
            Some(h3.to_str().unwrap()),
        );
        let models = load_models(tmp.path(), None);
        let names: Vec<_> = models.iter().map(|m| m.display_name.as_str()).collect();
        assert_eq!(names, vec!["Unique"]);
    }

    #[test]
    fn unknown_top_level_key_rejected() {
        let tmp = tempdir().unwrap();
        fs::write(
            tmp.path().join("bad.json"),
            r#"{
                "display_name": "Bad",
                "scope": "object_detection",
                "active": true,
                "input": {"width": 640, "height": 640, "format": "RGB"},
                "hef_path": "/tmp/x.hef",
                "postprocess": {
                    "so_path": "/tmp/lib.so",
                    "function_name": "f",
                    "output_format": "yolov8"
                },
                "postrprocess": "typo"
            }"#,
        )
        .unwrap();
        let models = load_models(tmp.path(), None);
        assert!(models.is_empty());
    }

    #[test]
    fn lookup_by_display_name() {
        let tmp = tempdir().unwrap();
        write_model(
            tmp.path(),
            "a.json",
            "Alpha",
            "object_detection",
            true,
            None,
        );
        let md = load_model_by_display_name(tmp.path(), "Alpha", None);
        assert!(md.is_some());
        assert_eq!(md.unwrap().display_name, "Alpha");
        assert!(load_model_by_display_name(tmp.path(), "Beta", None).is_none());
    }

    // ---------------------------------------------------------------
    // output_format family-defaults dispatch
    // ---------------------------------------------------------------

    /// Write a minimal model JSON that omits postprocess.so_path and
    /// postprocess.function_name - used to exercise the auto-fill path.
    fn write_minimal_postprocess(
        dir: &Path,
        filename: &str,
        display_name: &str,
        output_format: &str,
    ) {
        let hef = dir.join(format!("{}.hef", filename));
        fs::write(&hef, b"\x00").unwrap();
        let body = format!(
            r#"{{
                "display_name": "{name}",
                "scope": "object_detection",
                "active": true,
                "input": {{"width": 640, "height": 640, "format": "RGB"}},
                "hef_path": "{hef}",
                "postprocess": {{
                    "output_format": "{fmt}"
                }}
            }}"#,
            name = display_name,
            hef = hef.to_string_lossy(),
            fmt = output_format,
        );
        fs::write(dir.join(filename), body).unwrap();
    }

    #[test]
    fn family_defaults_table_mirrors_python() {
        // Guardrail: if this changes, update apps/model_registry.py too.
        assert_eq!(
            family_defaults("yolov5"),
            Some((TAPPAS_YOLO_SO, "filter_letterbox"))
        );
        assert_eq!(
            family_defaults("yolov8"),
            Some((TAPPAS_YOLO_SO, "filter_letterbox"))
        );
        assert_eq!(
            family_defaults("yolox"),
            Some((TAPPAS_YOLO_SO, "filter_letterbox"))
        );
        assert_eq!(
            family_defaults("yolo26"),
            Some((INTREE_YOLO26_SO, "yolo26"))
        );
        assert_eq!(family_defaults("custom"), None);
        assert_eq!(family_defaults("unknown"), None);
    }

    #[test]
    fn parse_one_auto_fills_yolov8_family_defaults() {
        let tmp = tempdir().unwrap();
        write_minimal_postprocess(tmp.path(), "a.json", "Auto YOLOv8", "yolov8");
        let md = parse_one(&tmp.path().join("a.json")).expect("should parse");
        assert_eq!(md.postprocess.so_path, TAPPAS_YOLO_SO);
        assert_eq!(md.postprocess.function_name, "filter_letterbox");
        assert_eq!(md.postprocess.output_format, "yolov8");
    }

    #[test]
    fn parse_one_auto_fills_yolo26_family_defaults() {
        let tmp = tempdir().unwrap();
        write_minimal_postprocess(tmp.path(), "a.json", "Auto YOLO26", "yolo26");
        let md = parse_one(&tmp.path().join("a.json")).expect("should parse");
        assert_eq!(md.postprocess.so_path, INTREE_YOLO26_SO);
        assert_eq!(md.postprocess.function_name, "yolo26");
    }

    #[test]
    fn parse_one_explicit_values_override_family_defaults() {
        let tmp = tempdir().unwrap();
        // write_model uses explicit so_path/function_name.
        write_model(
            tmp.path(),
            "a.json",
            "Explicit",
            "object_detection",
            true,
            None,
        );
        let md = parse_one(&tmp.path().join("a.json")).expect("should parse");
        // write_model defaults to function_name = "filter_letterbox"
        // which happens to also be the family default, so overwrite
        // the file with a non-default function_name to prove the
        // explicit value survives.
        let explicit_so = tmp.path().join("explicit.so");
        fs::write(&explicit_so, b"\x00").unwrap();
        let body = format!(
            r#"{{
                "display_name": "Override",
                "scope": "object_detection",
                "active": true,
                "input": {{"width": 640, "height": 640, "format": "RGB"}},
                "hef_path": "{hef}",
                "postprocess": {{
                    "so_path": "{so}",
                    "function_name": "custom_explicit_fn",
                    "output_format": "yolov8"
                }}
            }}"#,
            hef = md.hef_path.as_deref().unwrap_or(""),
            so = explicit_so.to_string_lossy(),
        );
        fs::write(tmp.path().join("b.json"), body).unwrap();
        let md2 = parse_one(&tmp.path().join("b.json")).expect("should parse");
        assert_eq!(md2.postprocess.so_path, explicit_so.to_string_lossy());
        assert_eq!(md2.postprocess.function_name, "custom_explicit_fn");
    }

    #[test]
    fn custom_family_without_explicit_fields_hidden_by_load() {
        // parse_one accepts it (postprocess has defaults), but load_models
        // hides it because hailo models need non-empty so_path.
        let tmp = tempdir().unwrap();
        write_minimal_postprocess(tmp.path(), "a.json", "Custom Missing", "custom");
        let md = parse_one(&tmp.path().join("a.json"));
        assert!(md.is_some(), "parse_one should accept (defaults fill in)");
        // But load_models should filter it out (empty so_path for hailo)
        let loaded = load_models(tmp.path(), None);
        assert!(
            loaded.is_empty(),
            "load_models should hide model with empty so_path"
        );
    }

    #[test]
    fn centerpose_family_without_explicit_fields_hidden_by_load() {
        let tmp = tempdir().unwrap();
        write_minimal_postprocess(tmp.path(), "a.json", "Centerpose Missing", "centerpose");
        let md = parse_one(&tmp.path().join("a.json"));
        assert!(md.is_some());
        let loaded = load_models(tmp.path(), None);
        assert!(loaded.is_empty());
    }

    #[test]
    fn parse_one_labels_index_map_round_trips() {
        // Serde-untagged tries variants in declaration order;
        // reordering Named(String) / IndexMap(Vec<String>) would
        // silently break the array form.
        let tmp = tempdir().unwrap();
        let hef = tmp.path().join("h.hef");
        fs::write(&hef, b"\x00").unwrap();
        let so = tmp.path().join("so.so");
        fs::write(&so, b"\x00").unwrap();
        let body = format!(
            r#"{{
                "display_name": "YOLO26s Labels Test",
                "scope": "object_detection",
                "active": true,
                "input": {{"width": 640, "height": 640, "format": "RGB"}},
                "hef_path": "{hef}",
                "postprocess": {{
                    "so_path": "{so}",
                    "function_name": "yolo26",
                    "output_format": "yolo26"
                }},
                "labels": ["ball", "robot", "human"]
            }}"#,
            hef = hef.to_string_lossy(),
            so = so.to_string_lossy(),
        );
        fs::write(tmp.path().join("a.json"), body).unwrap();
        let md = parse_one(&tmp.path().join("a.json")).expect("should parse");
        let labels = md.labels.expect("labels should be present");
        let idx = labels.as_index_map().expect("should be IndexMap");
        assert_eq!(idx, &["ball", "robot", "human"]);
    }

    #[test]
    fn parse_one_labels_named_string_round_trips() {
        let tmp = tempdir().unwrap();
        let hef = tmp.path().join("h.hef");
        fs::write(&hef, b"\x00").unwrap();
        let so = tmp.path().join("so.so");
        fs::write(&so, b"\x00").unwrap();
        let body = format!(
            r#"{{
                "display_name": "COCO Named",
                "scope": "object_detection",
                "active": true,
                "input": {{"width": 640, "height": 640, "format": "RGB"}},
                "hef_path": "{hef}",
                "postprocess": {{
                    "so_path": "{so}",
                    "function_name": "yolo26",
                    "output_format": "yolo26"
                }},
                "labels": "coco_80"
            }}"#,
            hef = hef.to_string_lossy(),
            so = so.to_string_lossy(),
        );
        fs::write(tmp.path().join("a.json"), body).unwrap();
        let md = parse_one(&tmp.path().join("a.json")).expect("should parse");
        let labels = md.labels.expect("labels should be present");
        assert!(matches!(labels, ModelLabels::Named(ref s) if s == "coco_80"));
        assert!(labels.as_index_map().is_none());
    }

    #[test]
    fn parse_one_partial_omission_fills_only_missing_field() {
        // function_name spelled out explicitly, so_path omitted - only
        // so_path should be filled in from the family default.
        let tmp = tempdir().unwrap();
        let hef = tmp.path().join("h.hef");
        fs::write(&hef, b"\x00").unwrap();
        let body = format!(
            r#"{{
                "display_name": "Partial",
                "scope": "object_detection",
                "active": true,
                "input": {{"width": 640, "height": 640, "format": "RGB"}},
                "hef_path": "{hef}",
                "postprocess": {{
                    "function_name": "my_custom_fn",
                    "output_format": "yolov5"
                }}
            }}"#,
            hef = hef.to_string_lossy(),
        );
        fs::write(tmp.path().join("a.json"), body).unwrap();
        let md = parse_one(&tmp.path().join("a.json")).expect("should parse");
        assert_eq!(md.postprocess.so_path, TAPPAS_YOLO_SO);
        assert_eq!(md.postprocess.function_name, "my_custom_fn");
    }
}
