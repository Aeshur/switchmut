use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// UI-thread-owned writer. Keep at most two 1 MiB logs; never log idle ticks.
pub struct Log {
    path: PathBuf,
}
impl Log {
    pub fn new(application_directory: &Path) -> Result<Self, String> {
        fs::create_dir_all(application_directory).map_err(|error| {
            format!(
                "portable application folder {} must be writable: {error}",
                application_directory.display()
            )
        })?;
        let path = application_directory.join("log-switchmut.log");
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|error| {
                format!(
                    "portable application folder {} must be writable: {error}",
                    application_directory.display()
                )
            })?;
        Ok(Self { path })
    }
    pub fn write(&self, message: impl AsRef<str>) {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let message = message.as_ref();
        let mut end = message.len().min(4096);
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        let line = format!(
            "{stamp} {}{}\n",
            &message[..end],
            if end < message.len() {
                " [truncated]"
            } else {
                ""
            }
        );
        if fs::metadata(&self.path).is_ok_and(|m| m.len() + line.len() as u64 > 1_048_576) {
            let previous = self.path.with_file_name("log-switchmut.previous.log");
            let _ = fs::remove_file(&previous);
            if fs::rename(&self.path, previous).is_err() {
                return;
            }
        }
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = file.write_all(line.as_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn logs_rotate_and_bound_large_native_error_messages() {
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-artifacts/log-tests")
            .join(format!(
                "{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        let log = Log::new(&directory).expect("create writable log");
        assert_eq!(
            log.path.file_name().and_then(|name| name.to_str()),
            Some("log-switchmut.log")
        );
        fs::write(&log.path, vec![b'x'; 1_048_576]).unwrap();
        log.write("a".repeat(2_000_000));
        assert_eq!(
            fs::metadata(log.path.with_file_name("log-switchmut.previous.log"))
                .unwrap()
                .len(),
            1_048_576
        );
        let text = fs::read_to_string(&log.path).unwrap();
        assert!(text.len() < 4200);
        assert!(text.ends_with(" [truncated]\n"));
    }
}
