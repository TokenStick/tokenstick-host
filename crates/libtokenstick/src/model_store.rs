//! Local model store.
//!
//! An RKNN3 LLM is a set of files, not one file. Each model lives in its own
//! directory with a `manifest.json` that names them:
//!
//! ```text
//! <root>/<model-id>/
//!   manifest.json
//!   <name>.rknn            graph, runs on the NPU
//!   <name>.weight          weights, paired with .rknn
//!   <name>.tokenizer.gguf  tokenizer, host
//!   <name>.embed.bin       FP16 embedding table, host
//! ```
//!
//! The root is `$TOKENSTICK_MODELS_DIR`, else `$XDG_DATA_HOME/tokenstick/models`,
//! else `~/.local/share/tokenstick/models`. tokenstick-models produces this layout.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, ModelInfo, Result};

pub const MANIFEST: &str = "manifest.json";

/// Files that make up one converted model, relative to its directory.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelFiles {
    pub graph: String,
    pub weights: String,
    pub tokenizer: String,
    pub embeddings: String,
}

impl ModelFiles {
    fn all(&self) -> [&str; 4] {
        [
            &self.graph,
            &self.weights,
            &self.tokenizer,
            &self.embeddings,
        ]
    }
}

/// Contents of `manifest.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub id: String,
    pub family: String,
    pub params_b: f32,
    pub quantization: String,
    pub context_len: u32,
    #[serde(default)]
    pub host_mem_mib: Option<u32>,
    /// Chip the model was compiled for, e.g. `RK1828`.
    pub target: String,
    /// Chat template passed to the runtime; `None` uses the runtime default.
    #[serde(default)]
    pub chat_template: Option<String>,
    pub files: ModelFiles,
}

impl Manifest {
    pub fn info(&self) -> ModelInfo {
        ModelInfo {
            id: self.id.clone(),
            family: self.family.clone(),
            params_b: self.params_b,
            quantization: self.quantization.clone(),
            context_len: self.context_len,
            host_mem_mib: self.host_mem_mib,
        }
    }
}

/// A model found in the store.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredModel {
    pub dir: PathBuf,
    pub manifest: Manifest,
}

impl StoredModel {
    /// Absolute path of a file named in the manifest.
    pub fn path(&self, file: &str) -> PathBuf {
        self.dir.join(file)
    }

    /// Manifest files that are not on disk. Empty means the model is runnable.
    pub fn missing_files(&self) -> Vec<String> {
        self.manifest
            .files
            .all()
            .into_iter()
            .filter(|f| !self.path(f).is_file())
            .map(str::to_owned)
            .collect()
    }
}

/// Result of scanning the store: valid entries plus directories that could not be read.
#[derive(Debug, Default)]
pub struct ScanReport {
    pub models: Vec<StoredModel>,
    pub problems: Vec<(PathBuf, String)>,
}

#[derive(Debug, Clone)]
pub struct ModelStore {
    root: PathBuf,
}

impl ModelStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Store at the default location (see module docs).
    pub fn default_location() -> Result<Self> {
        if let Some(dir) = std::env::var_os("TOKENSTICK_MODELS_DIR") {
            return Ok(Self::new(dir));
        }
        let data = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .ok_or_else(|| {
                Error::InvalidRequest(
                    "cannot locate model store: set TOKENSTICK_MODELS_DIR or HOME".into(),
                )
            })?;
        Ok(Self::new(data.join("tokenstick").join("models")))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Read every `<root>/<dir>/manifest.json`. A missing root is an empty store.
    pub fn scan(&self) -> Result<ScanReport> {
        let mut report = ScanReport::default();
        let entries = match fs::read_dir(&self.root) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(report),
            Err(e) => {
                return Err(Error::Backend(format!(
                    "reading {}: {e}",
                    self.root.display()
                )))
            }
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            match read_manifest(&dir) {
                Ok(manifest) => report.models.push(StoredModel { dir, manifest }),
                Err(msg) => report.problems.push((dir, msg)),
            }
        }
        report
            .models
            .sort_by(|a, b| a.manifest.id.cmp(&b.manifest.id));
        Ok(report)
    }

    /// Find one model by id.
    pub fn get(&self, id: &str) -> Result<StoredModel> {
        self.scan()?
            .models
            .into_iter()
            .find(|m| m.manifest.id == id)
            .ok_or_else(|| Error::ModelNotFound(id.into()))
    }
}

fn read_manifest(dir: &Path) -> std::result::Result<Manifest, String> {
    let path = dir.join(MANIFEST);
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!(
                "tokenstick-store-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn manifest(id: &str) -> Manifest {
        Manifest {
            id: id.into(),
            family: "qwen2.5".into(),
            params_b: 3.0,
            quantization: "w4a16".into(),
            context_len: 4096,
            host_mem_mib: Some(593),
            target: "RK1828".into(),
            chat_template: None,
            files: ModelFiles {
                graph: "m.rknn".into(),
                weights: "m.weight".into(),
                tokenizer: "m.tokenizer.gguf".into(),
                embeddings: "m.embed.bin".into(),
            },
        }
    }

    fn write_model(root: &Path, id: &str, with_files: bool) {
        let dir = root.join(id);
        fs::create_dir_all(&dir).unwrap();
        let m = manifest(id);
        fs::write(
            dir.join(MANIFEST),
            serde_json::to_string_pretty(&m).unwrap(),
        )
        .unwrap();
        if with_files {
            for f in m.files.all() {
                fs::write(dir.join(f), b"x").unwrap();
            }
        }
    }

    #[test]
    fn missing_root_is_empty() {
        let store = ModelStore::new("/nonexistent/tokenstick/models");
        let r = store.scan().unwrap();
        assert!(r.models.is_empty() && r.problems.is_empty());
    }

    #[test]
    fn scans_models_and_reports_problems() {
        let tmp = TempDir::new();
        write_model(&tmp.0, "qwen2.5-3b", true);
        write_model(&tmp.0, "qwen2.5-1.5b", false);
        fs::create_dir_all(tmp.0.join("broken")).unwrap();
        fs::write(tmp.0.join("broken").join(MANIFEST), "{ not json").unwrap();

        let store = ModelStore::new(&tmp.0);
        let r = store.scan().unwrap();
        let ids: Vec<_> = r.models.iter().map(|m| m.manifest.id.as_str()).collect();
        assert_eq!(ids, ["qwen2.5-1.5b", "qwen2.5-3b"]);
        assert_eq!(r.problems.len(), 1);
        assert_eq!(r.models[0].missing_files().len(), 4);
        assert!(r.models[1].missing_files().is_empty());

        let m = store.get("qwen2.5-3b").unwrap();
        assert_eq!(m.manifest.info().host_mem_mib, Some(593));
        assert!(matches!(store.get("nope"), Err(Error::ModelNotFound(_))));
    }
}
