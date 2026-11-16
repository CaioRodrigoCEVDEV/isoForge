use crate::core::error::{CoreError, CoreResult};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Clone, Debug)]
pub struct TreeFile {
    pub relative_path: String,
    pub size: u64,
}

pub fn scan_tree(root: &Path) -> (u64, u64) {
    let mut files = 0u64;
    let mut bytes = 0u64;

    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        if entry.file_type().is_file() {
            files += 1;
            if let Ok(metadata) = entry.metadata() {
                bytes = bytes.saturating_add(metadata.len());
            }
        }
    }

    (files, bytes)
}

pub fn tree_files(root: &Path) -> CoreResult<Vec<TreeFile>> {
    let mut files = Vec::new();

    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|e| {
            CoreError::new(format!(
                "Falha ao percorrer o workspace '{}': {e}",
                root.display()
            ))
        })?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|e| CoreError::new(e.to_string()))?
            .to_string_lossy()
            .replace('\\', "/");
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        files.push(TreeFile {
            relative_path: format!("/{relative}"),
            size,
        });
    }

    files.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    Ok(files)
}

/// Makes an extracted tree writable by the user. ISOs often carry read-only
/// permissions, which would block removing files and deleting the workspace.
pub fn ensure_writable_tree(root: &Path) -> CoreResult<()> {
    use std::os::unix::fs::PermissionsExt;

    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|e| {
            CoreError::new(format!(
                "Falha ao ajustar permissões em '{}': {e}",
                root.display()
            ))
        })?;
        let file_type = entry.file_type();
        if file_type.is_symlink() {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        let mut permissions = metadata.permissions();
        let mode = permissions.mode();
        let new_mode = if file_type.is_dir() {
            mode | 0o700
        } else {
            mode | 0o200
        };
        if new_mode != mode {
            permissions.set_mode(new_mode);
            let _ = std::fs::set_permissions(entry.path(), permissions);
        }
    }

    Ok(())
}

pub fn iso_rr_to_relative(path: &str) -> String {
    path.trim_start_matches('/').replace('\\', "/")
}

pub fn join_iso_path(root: &Path, iso_path: &str) -> PathBuf {
    let mut out = root.to_path_buf();
    for component in iso_rr_to_relative(iso_path).split('/') {
        if !component.is_empty() {
            out.push(component);
        }
    }
    out
}
