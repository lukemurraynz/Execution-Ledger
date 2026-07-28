use std::fs;
use std::path::PathBuf;

use serde_json::Value;
use uuid::Uuid;

use crate::error::{ExecutionLedgerError, Result};

const DEFAULT_INLINE_MAX_BYTES: usize = 5_120;

#[derive(Debug, Clone)]
pub struct PayloadOffload {
    dir: Option<PathBuf>,
    max_inline_bytes: usize,
}

impl PayloadOffload {
    pub fn from_env() -> Result<Self> {
        let dir = std::env::var("EXECUTION_LEDGER_OFFLOAD_DIR")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from);
        let max_inline_bytes = std::env::var("EXECUTION_LEDGER_OFFLOAD_INLINE_MAX_BYTES")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(DEFAULT_INLINE_MAX_BYTES);
        Ok(Self {
            dir,
            max_inline_bytes,
        })
    }

    pub fn store_if_needed(&self, run_id: Uuid, payload: &Value) -> Result<Value> {
        let bytes = serde_json::to_vec(payload).map_err(|source| {
            ExecutionLedgerError::DependencyFailed {
                dependency: "serde_json",
                source: Box::new(source),
            }
        })?;
        if bytes.len() <= self.max_inline_bytes || self.dir.is_none() {
            return Ok(payload.clone());
        }

        let dir = self
            .dir
            .as_ref()
            .ok_or(ExecutionLedgerError::InvalidConfiguration {
                name: "EXECUTION_LEDGER_OFFLOAD_DIR".to_string(),
            })?;
        fs::create_dir_all(dir).map_err(|source| ExecutionLedgerError::DependencyFailed {
            dependency: "filesystem",
            source: Box::new(source),
        })?;
        let path = dir.join(format!("{run_id}.json"));
        let tmp_path = dir.join(format!("{run_id}.json.tmp"));
        fs::write(&tmp_path, &bytes).map_err(|source| ExecutionLedgerError::DependencyFailed {
            dependency: "filesystem",
            source: Box::new(source),
        })?;
        fs::rename(&tmp_path, &path).map_err(|source| ExecutionLedgerError::DependencyFailed {
            dependency: "filesystem",
            source: Box::new(source),
        })?;

        Ok(serde_json::json!({
            "offloadPath": path.to_string_lossy(),
            "outputStatus": "offloaded"
        }))
    }
}

pub fn cleanup_offload_files(paths: &[String]) -> Result<u64> {
    let mut deleted = 0_u64;
    for path in paths {
        match fs::remove_file(path) {
            Ok(()) => deleted += 1,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(ExecutionLedgerError::DependencyFailed {
                    dependency: "filesystem",
                    source: Box::new(source),
                });
            }
        }
    }
    Ok(deleted)
}
