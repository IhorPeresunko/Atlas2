use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use tokio::fs;

use crate::{
    domain::{DirectoryEntry, WorkspacePath},
    error::{AppError, AppResult},
};

#[derive(Clone, Default)]
pub struct FilesystemService {
    _private: Arc<()>,
}

impl FilesystemService {
    pub async fn normalize_directory(&self, raw_path: &str) -> AppResult<WorkspacePath> {
        if !raw_path.starts_with('/') {
            return Err(AppError::Validation(
                "workspace path must be an absolute path".into(),
            ));
        }

        let canonical = fs::canonicalize(raw_path).await?;
        let metadata = fs::metadata(&canonical).await?;
        if !metadata.is_dir() {
            return Err(AppError::Validation(format!(
                "workspace path is not a directory: {}",
                canonical.display()
            )));
        }

        Ok(WorkspacePath(canonical.to_string_lossy().into_owned()))
    }

    pub async fn list_directory(
        &self,
        raw_path: &str,
        max_entries: usize,
    ) -> AppResult<Vec<DirectoryEntry>> {
        let path = self.normalize_directory(raw_path).await?;
        let mut reader = fs::read_dir(&path.0).await?;
        let mut entries = Vec::new();

        while let Some(entry) = reader.next_entry().await? {
            let file_type = entry.file_type().await?;
            let entry_path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();

            entries.push(DirectoryEntry {
                path: WorkspacePath(entry_path.to_string_lossy().into_owned()),
                name,
                is_dir: file_type.is_dir(),
            });
        }

        entries.sort_by(|left, right| {
            right
                .is_dir
                .cmp(&left.is_dir)
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
        });
        entries.truncate(max_entries);
        Ok(entries)
    }

    pub fn parent_directory(&self, raw_path: &str) -> Option<WorkspacePath> {
        let path = PathBuf::from(raw_path);
        path.parent()
            .map(Path::to_path_buf)
            .map(|value| WorkspacePath(value.to_string_lossy().into_owned()))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::FilesystemService;

    #[tokio::test]
    async fn lists_directories_before_files() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("b_dir")).unwrap();
        fs::create_dir(dir.path().join("a_dir")).unwrap();
        fs::write(dir.path().join("c.txt"), "ok").unwrap();

        let service = FilesystemService::default();
        let entries = service
            .list_directory(dir.path().to_str().unwrap(), 20)
            .await
            .unwrap();

        assert_eq!(entries[0].name, "a_dir");
        assert!(entries[0].is_dir);
        assert_eq!(entries[1].name, "b_dir");
        assert!(entries[1].is_dir);
        assert_eq!(entries[2].name, "c.txt");
        assert!(!entries[2].is_dir);
    }
}
