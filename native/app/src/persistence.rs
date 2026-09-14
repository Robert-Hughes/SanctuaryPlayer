use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

pub(crate) fn write_atomic(path: &Path, contents: &[u8]) -> Result<(), String> {
    let Some(parent) = path.parent() else {
        return Err(format!(
            "persistence path has no parent: {}",
            path.display()
        ));
    };
    fs::create_dir_all(parent)
        .map_err(|error| format!("create persistence directory {}: {error}", parent.display()))?;

    let temp_path = temporary_path(path);
    let result = (|| {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temp_path)
            .map_err(|error| format!("create temporary file {}: {error}", temp_path.display()))?;
        file.write_all(contents)
            .map_err(|error| format!("write temporary file {}: {error}", temp_path.display()))?;
        file.sync_all()
            .map_err(|error| format!("sync temporary file {}: {error}", temp_path.display()))?;
        drop(file);

        fs::rename(&temp_path, path).map_err(|error| {
            format!(
                "replace {} with {}: {error}",
                path.display(),
                temp_path.display()
            )
        })?;

        sync_parent_directory(parent)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn temporary_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state");
    path.with_file_name(format!(".{file_name}.tmp-{}", std::process::id()))
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> Result<(), String> {
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("sync persistence directory {}: {error}", parent.display()))
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temporary_path_for_test(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir()
            .join(format!(
                "sanctuary-player-atomic-test-{}-{unique}",
                std::process::id()
            ))
            .join(name)
    }

    #[test]
    fn atomic_write_creates_and_replaces_file() {
        let path = temporary_path_for_test("state.json");
        write_atomic(&path, b"first\n").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"first\n");

        write_atomic(&path, b"second\n").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second\n");
        assert!(!temporary_path(&path).exists());

        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
