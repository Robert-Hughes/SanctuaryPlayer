use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use chrono::{DateTime, SecondsFormat, Utc};
use log::{LevelFilter, Log, Metadata, Record};

const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;
const MAX_LOG_FILES: usize = 20;
const LOG_PREFIX: &str = "sanctuary-player-";

#[derive(Debug)]
pub enum LoggingError {
    Io(io::Error),
    GlobalLoggerAlreadySet,
}

impl fmt::Display for LoggingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::GlobalLoggerAlreadySet => write!(f, "global logger is already initialised"),
        }
    }
}

impl Error for LoggingError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::GlobalLoggerAlreadySet => None,
        }
    }
}

impl From<io::Error> for LoggingError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

struct FileLogger {
    writer: Mutex<RotatingWriter>,
}

impl Log for FileLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let now =
            DateTime::<Utc>::from(SystemTime::now()).to_rfc3339_opts(SecondsFormat::Millis, true);
        let thread = std::thread::current();
        let thread_name = thread.name().unwrap_or("unnamed");
        let line = format!(
            "[{now}] {:<5} [{thread_name}] {}: {}\n",
            record.level(),
            record.target(),
            record.args()
        );

        if let Ok(mut writer) = self.writer.lock() {
            let _ = writer.write_line(line.as_bytes());
        }
    }

    fn flush(&self) {
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writer.file.flush();
        }
    }
}

struct RotatingWriter {
    directory: PathBuf,
    launch_base: String,
    part: u32,
    file: File,
    current_path: PathBuf,
    current_bytes: u64,
    max_bytes: u64,
    max_files: usize,
}

impl RotatingWriter {
    fn new(
        directory: PathBuf,
        launch_base: String,
        max_bytes: u64,
        max_files: usize,
    ) -> io::Result<Self> {
        fs::create_dir_all(&directory)?;
        let (file, current_path) = open_log_part(&directory, &launch_base, 1)?;
        let mut writer = Self {
            directory,
            launch_base,
            part: 1,
            file,
            current_path,
            current_bytes: 0,
            max_bytes,
            max_files,
        };
        writer.prune_old_logs()?;
        Ok(writer)
    }

    fn write_line(&mut self, bytes: &[u8]) -> io::Result<()> {
        let incoming = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if self.current_bytes != 0 && self.current_bytes.saturating_add(incoming) > self.max_bytes {
            self.rotate()?;
        }

        self.file.write_all(bytes)?;
        self.current_bytes = self.current_bytes.saturating_add(incoming);
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file.flush()?;
        self.part = self.part.saturating_add(1);
        let (file, path) = open_log_part(&self.directory, &self.launch_base, self.part)?;
        self.file = file;
        self.current_path = path;
        self.current_bytes = 0;
        self.prune_old_logs()
    }

    fn prune_old_logs(&mut self) -> io::Result<()> {
        let mut logs = fs::read_dir(&self.directory)?
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let file_type = entry.file_type().ok()?;
                if !file_type.is_file() {
                    return None;
                }
                let name = entry.file_name();
                let name = name.to_str()?;
                (name.starts_with(LOG_PREFIX) && name.ends_with(".log")).then_some(entry.path())
            })
            .collect::<Vec<_>>();

        logs.sort();
        let mut remove_count = logs.len().saturating_sub(self.max_files);
        for path in logs {
            if remove_count == 0 {
                break;
            }
            if path == self.current_path {
                continue;
            }
            if fs::remove_file(path).is_ok() {
                remove_count -= 1;
            }
        }
        Ok(())
    }
}

fn open_log_part(directory: &Path, launch_base: &str, part: u32) -> io::Result<(File, PathBuf)> {
    let path = directory.join(format!("{launch_base}-part{part:08}.log"));
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)?;
    Ok((file, path))
}

/// Initialise SanctuaryPlayer's process-wide runtime logger.
///
/// The caller supplies a platform-appropriate directory. Each launch gets a
/// timestamp/PID-specific filename; a long-running launch rolls to further
/// part files once a segment reaches 8 MiB. Only the newest 20 Sanctuary log
/// segments are retained in the directory.
pub fn init(directory: impl AsRef<Path>) -> Result<PathBuf, LoggingError> {
    let directory = directory.as_ref().to_path_buf();
    let stamp = DateTime::<Utc>::from(SystemTime::now()).format("%Y%m%d-%H%M%S%.3fZ");
    let launch_base = format!("{LOG_PREFIX}{stamp}-p{}", std::process::id());
    let writer = RotatingWriter::new(directory, launch_base, MAX_LOG_BYTES, MAX_LOG_FILES)?;
    let first_path = writer.current_path.clone();

    let logger = Box::leak(Box::new(FileLogger {
        writer: Mutex::new(writer),
    }));
    log::set_logger(logger).map_err(|_| LoggingError::GlobalLoggerAlreadySet)?;
    log::set_max_level(LevelFilter::Info);

    log::info!(
        "SanctuaryPlayer: logging initialised file={} segment_limit={} bytes retained_segments={}",
        first_path.display(),
        MAX_LOG_BYTES,
        MAX_LOG_FILES
    );
    Ok(first_path)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    fn test_directory(name: &str) -> PathBuf {
        let sequence = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "sanctuary-player-log-test-{name}-{}-{sequence}",
            std::process::id()
        ))
    }

    fn log_files(directory: &Path) -> Vec<PathBuf> {
        let mut files = fs::read_dir(directory)
            .unwrap()
            .map(Result::unwrap)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "log"))
            .collect::<Vec<_>>();
        files.sort();
        files
    }

    #[test]
    fn rotates_before_a_segment_exceeds_the_limit() {
        let directory = test_directory("rotate");
        let mut writer =
            RotatingWriter::new(directory.clone(), "sanctuary-player-test".into(), 10, 20).unwrap();

        writer.write_line(b"12345\n").unwrap();
        writer.write_line(b"67890\n").unwrap();
        writer.file.flush().unwrap();

        let files = log_files(&directory);
        assert_eq!(files.len(), 2);
        assert_eq!(fs::read_to_string(&files[0]).unwrap(), "12345\n");
        assert_eq!(fs::read_to_string(&files[1]).unwrap(), "67890\n");

        drop(writer);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn retains_only_the_newest_log_segments() {
        let directory = test_directory("retention");
        let mut writer =
            RotatingWriter::new(directory.clone(), "sanctuary-player-test".into(), 5, 3).unwrap();

        for index in 0..6 {
            writer
                .write_line(format!("{index:04}\n").as_bytes())
                .unwrap();
        }
        writer.file.flush().unwrap();

        let files = log_files(&directory);
        assert_eq!(files.len(), 3);
        assert!(
            files[0]
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains("part00000004")
        );
        assert!(
            files[2]
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains("part00000006")
        );

        drop(writer);
        fs::remove_dir_all(directory).unwrap();
    }
}
