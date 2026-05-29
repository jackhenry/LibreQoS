use anyhow::{Error, Result};
use nix::{
    errno::Errno,
    libc::{getpid, mode_t},
};
use std::{
    ffi::CString,
    fs::{File, OpenOptions, hard_link, remove_file},
    io::{ErrorKind, Read, Write},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

const LOCK_PATH: &str = "/run/lqos/lqos_overrides.lock";
const LOCK_DIR: &str = "/run/lqos";
const LOCK_DIR_PERMS: &str = "/run/lqos";
const LOCK_GUARD_PATH: &str = "/run/lqos/lqos_overrides.lock.guard";
const STALE_LOCK_REPLACE_ATTEMPTS: usize = 3;
const LOCK_TEMP_CREATE_ATTEMPTS: usize = 8;
const LOCK_CONTENTION_CODE: &str = "LQOS_OVERRIDES_LOCKED";

static TEMP_LOCK_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Cross-process lock used while mutating operator-owned override files.
#[derive(Debug)]
pub struct FileLock {
    lock_path: PathBuf,
}

struct AcquisitionGuard {
    file: File,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LockMetadata {
    pid: i32,
    process: Option<String>,
    operation: Option<String>,
    created_unix: Option<u64>,
}

impl LockMetadata {
    fn current(operation: &str) -> Self {
        Self {
            pid: unsafe { getpid() },
            process: current_process_name(),
            operation: Some(sanitize_lock_field(operation)),
            created_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()
                .map(|duration| duration.as_secs()),
        }
    }

    fn parse(contents: &str) -> Result<Self> {
        let trimmed = contents.trim();
        if let Ok(pid) = trimmed.parse::<i32>() {
            return Ok(Self {
                pid,
                process: None,
                operation: None,
                created_unix: None,
            });
        }

        let mut pid = None;
        let mut process = None;
        let mut operation = None;
        let mut created_unix = None;
        for line in contents.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key.trim() {
                "pid" => pid = Some(value.trim().parse::<i32>()?),
                "process" => process = Some(value.trim().to_string()),
                "operation" => operation = Some(value.trim().to_string()),
                "created_unix" => created_unix = Some(value.trim().parse::<u64>()?),
                _ => {}
            }
        }

        let Some(pid) = pid else {
            return Err(Error::msg(
                "The LibreQoS overrides lock file does not contain a process id.",
            ));
        };

        Ok(Self {
            pid,
            process,
            operation,
            created_unix,
        })
    }

    fn serialize(&self) -> String {
        let process = self.process.as_deref().unwrap_or("unknown");
        let operation = self.operation.as_deref().unwrap_or("unknown");
        let created_unix = self
            .created_unix
            .map(|seconds| seconds.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        format!(
            "pid={}\nprocess={}\noperation={}\ncreated_unix={}\n",
            self.pid, process, operation, created_unix
        )
    }

    fn describe_holder(&self) -> String {
        let mut parts = vec![format!("pid={}", self.pid)];
        if let Some(process) = self.process.as_deref().filter(|value| !value.is_empty()) {
            parts.push(format!("process={process}"));
        }
        if let Some(operation) = self.operation.as_deref().filter(|value| !value.is_empty()) {
            parts.push(format!("operation={operation}"));
        }
        if let Some(created_unix) = self.created_unix {
            parts.push(format!("created_unix={created_unix}"));
        }
        parts.join(", ")
    }
}

impl FileLock {
    /// Acquires the lock and records the caller operation for diagnostics.
    pub fn new_for_operation(operation: &str) -> Result<Self> {
        Self::new_at(
            operation,
            Path::new(LOCK_PATH),
            Path::new(LOCK_DIR),
            Path::new(LOCK_DIR_PERMS),
            Path::new(LOCK_GUARD_PATH),
        )
    }

    fn new_at(
        operation: &str,
        lock_path: &Path,
        lock_dir: &Path,
        lock_dir_perms: &Path,
        guard_path: &Path,
    ) -> Result<Self> {
        Self::check_directory_at(lock_dir, lock_dir_perms)?;
        let _guard = Self::acquire_guard(guard_path)?;
        for _ in 0..STALE_LOCK_REPLACE_ATTEMPTS {
            if Self::create_lock(lock_path, lock_dir, operation)? {
                return Ok(Self {
                    lock_path: lock_path.to_path_buf(),
                });
            }

            let metadata = match Self::read_lock_metadata(lock_path) {
                Ok(metadata) => metadata,
                Err(err) if Self::is_not_found_error(&err) => continue,
                Err(err) => {
                    return Err(Error::msg(format!(
                        "{LOCK_CONTENTION_CODE}: The LibreQoS overrides files are locked by another process (lock metadata unreadable: {err})."
                    )));
                }
            };
            if Self::is_lock_valid(&metadata) {
                return Err(Error::msg(format!(
                    "{LOCK_CONTENTION_CODE}: The LibreQoS overrides files are locked by another process ({}).",
                    metadata.describe_holder()
                )));
            }

            match remove_file(lock_path) {
                Ok(()) => {}
                Err(err) if err.kind() == ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
        }

        Err(Error::msg(
            "Unable to acquire the LibreQoS overrides lock after replacing a stale lock.",
        ))
    }

    fn acquire_guard(guard_path: &Path) -> Result<AcquisitionGuard> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(guard_path)?;
        let unix_path = CString::new(guard_path.to_string_lossy().as_bytes())?;
        unsafe {
            nix::libc::chmod(unix_path.as_ptr(), mode_t::from_le(0o666));
        }
        let ret = unsafe { nix::libc::flock(file.as_raw_fd(), nix::libc::LOCK_EX) };
        if ret == 0 {
            Ok(AcquisitionGuard { file })
        } else {
            Err(Error::msg(format!(
                "Unable to acquire LibreQoS overrides lock guard: {}",
                Errno::last()
            )))
        }
    }

    fn read_lock_metadata(lock_path: &Path) -> Result<LockMetadata> {
        let mut f = File::open(lock_path)?;
        let mut contents = String::new();
        f.read_to_string(&mut contents)?;
        LockMetadata::parse(&contents)
    }

    fn is_not_found_error(err: &Error) -> bool {
        err.downcast_ref::<std::io::Error>()
            .is_some_and(|io_err| io_err.kind() == ErrorKind::NotFound)
    }

    fn is_lock_valid(metadata: &LockMetadata) -> bool {
        let ret = unsafe { nix::libc::kill(metadata.pid, 0) };
        if ret == 0 {
            return true;
        }
        let err = Errno::last();
        err != Errno::ESRCH
    }

    fn create_lock(lock_path: &Path, lock_dir: &Path, operation: &str) -> Result<bool> {
        let metadata = LockMetadata::current(operation);
        let (mut f, temp_path) = Self::create_temp_lock_file(lock_dir)?;
        f.write_all(metadata.serialize().as_bytes())?;
        f.sync_all()?;
        drop(f);
        let link_result = hard_link(&temp_path, lock_path);
        let _ = remove_file(&temp_path);
        match link_result {
            Ok(()) => {}
            Err(err) if err.kind() == ErrorKind::AlreadyExists => return Ok(false),
            Err(err) => return Err(err.into()),
        }
        let unix_path = CString::new(lock_path.to_string_lossy().as_bytes())?;
        unsafe {
            nix::libc::chmod(unix_path.as_ptr(), mode_t::from_le(0o666));
        }
        Ok(true)
    }

    fn create_temp_lock_file(lock_dir: &Path) -> Result<(File, PathBuf)> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        for _ in 0..LOCK_TEMP_CREATE_ATTEMPTS {
            let sequence = TEMP_LOCK_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let temp_path = lock_dir.join(format!(
                ".lqos_overrides.lock.{}.{}.{}.tmp",
                std::process::id(),
                nanos,
                sequence
            ));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp_path)
            {
                Ok(file) => return Ok((file, temp_path)),
                Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(err.into()),
            }
        }

        Err(Error::msg(format!(
            "Unable to create a unique temporary LibreQoS overrides lock file after {LOCK_TEMP_CREATE_ATTEMPTS} attempts."
        )))
    }

    fn check_directory_at(lock_dir: &Path, lock_dir_perms: &Path) -> Result<()> {
        if lock_dir.exists() && lock_dir.is_dir() {
            Ok(())
        } else {
            std::fs::create_dir(lock_dir)?;
            let unix_path = CString::new(lock_dir_perms.to_string_lossy().as_bytes())?;
            unsafe {
                nix::libc::chmod(unix_path.as_ptr(), 0o777);
            }
            Ok(())
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = remove_file(&self.lock_path);
    }
}

impl Drop for AcquisitionGuard {
    fn drop(&mut self) {
        let _ = unsafe { nix::libc::flock(self.file.as_raw_fd(), nix::libc::LOCK_UN) };
    }
}

fn current_process_name() -> Option<String> {
    std::fs::read_to_string("/proc/self/comm")
        .ok()
        .and_then(|name| {
            let trimmed = sanitize_lock_field(name.trim());
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        })
        .or_else(|| {
            std::env::args().next().and_then(|arg| {
                let trimmed = sanitize_lock_field(&arg);
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                }
            })
        })
}

fn sanitize_lock_field(value: &str) -> String {
    value
        .chars()
        .filter(|character| *character != '\n' && *character != '\r')
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{FileLock, LockMetadata};
    use std::{
        fs::{create_dir_all, read_to_string, remove_dir_all, write},
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn unique_test_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before UNIX_EPOCH")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "lqos-overrides-lock-test-{}-{nanos}",
            std::process::id()
        ))
    }

    #[test]
    fn parses_legacy_pid_only_lock_file() {
        let metadata = LockMetadata::parse("12345\n").expect("legacy pid lock should parse");

        assert_eq!(metadata.pid, 12345);
        assert_eq!(metadata.process, None);
        assert_eq!(metadata.operation, None);
        assert_eq!(metadata.created_unix, None);
    }

    #[test]
    fn parses_metadata_lock_file() {
        let metadata = LockMetadata::parse(
            "pid=12345\nprocess=lqosd\noperation=load effective overrides\ncreated_unix=1800000000\n",
        )
        .expect("metadata lock should parse");

        assert_eq!(metadata.pid, 12345);
        assert_eq!(metadata.process.as_deref(), Some("lqosd"));
        assert_eq!(
            metadata.operation.as_deref(),
            Some("load effective overrides")
        );
        assert_eq!(metadata.created_unix, Some(1_800_000_000));
    }

    #[test]
    fn holder_description_includes_available_metadata() {
        let metadata = LockMetadata::parse(
            "pid=12345\nprocess=node_manager\noperation=save overrides\ncreated_unix=1800000000\n",
        )
        .expect("metadata lock should parse");

        assert_eq!(
            metadata.describe_holder(),
            "pid=12345, process=node_manager, operation=save overrides, created_unix=1800000000"
        );
    }

    #[test]
    fn lock_file_records_operation_metadata_and_cleans_up_on_drop() {
        let dir = unique_test_dir();
        create_dir_all(&dir).expect("failed to create temp test dir");
        let path = dir.join("lqos_overrides.lock");

        {
            let _lock = FileLock::new_at(
                "load effective overrides",
                &path,
                &dir,
                &dir,
                &dir.join("lqos_overrides.lock.guard"),
            )
            .expect("failed to create lock in temp dir");
            let contents = read_to_string(&path).expect("failed to read temp lock");

            assert!(contents.contains(&format!("pid={}", std::process::id())));
            assert!(contents.contains("operation=load effective overrides"));
            assert!(contents.contains("created_unix="));
        }

        assert!(!path.exists());
        remove_dir_all(&dir).expect("failed to clean up temp test dir");
    }

    #[test]
    fn live_lock_error_reports_existing_holder_metadata() {
        let dir = unique_test_dir();
        create_dir_all(&dir).expect("failed to create temp test dir");
        let path = dir.join("lqos_overrides.lock");
        write(
            &path,
            format!(
                "pid={}\nprocess=test-holder\noperation=save overrides\ncreated_unix=1800000000\n",
                std::process::id()
            ),
        )
        .expect("failed to write temp lock");

        let error = FileLock::new_at(
            "load effective overrides",
            &path,
            &dir,
            &dir,
            &dir.join("lqos_overrides.lock.guard"),
        )
        .expect_err("live lock should reject a second holder");
        let message = error.to_string();

        assert!(message.contains("pid="));
        assert!(message.contains("process=test-holder"));
        assert!(message.contains("operation=save overrides"));
        assert!(message.contains("created_unix=1800000000"));

        remove_dir_all(&dir).expect("failed to clean up temp test dir");
    }

    #[test]
    fn malformed_lock_file_is_reported_as_lock_contention() {
        let dir = unique_test_dir();
        create_dir_all(&dir).expect("failed to create temp test dir");
        let path = dir.join("lqos_overrides.lock");
        write(&path, "pid=").expect("failed to write malformed temp lock");

        let error = FileLock::new_at(
            "load effective overrides",
            &path,
            &dir,
            &dir,
            &dir.join("lqos_overrides.lock.guard"),
        )
        .expect_err("malformed lock metadata should be reported as contention");
        let message = error.to_string();

        assert!(message.contains("locked by another process"));
        assert!(message.contains("lock metadata unreadable"));

        remove_dir_all(&dir).expect("failed to clean up temp test dir");
    }

    #[test]
    fn truncated_metadata_lock_missing_pid_is_reported_as_contention() {
        let dir = unique_test_dir();
        create_dir_all(&dir).expect("failed to create temp test dir");
        let path = dir.join("lqos_overrides.lock");
        write(
            &path,
            "process=test-holder\noperation=save overrides\ncreated_unix=1800000000\n",
        )
        .expect("failed to write truncated temp lock");

        let error = FileLock::new_at(
            "load effective overrides",
            &path,
            &dir,
            &dir,
            &dir.join("lqos_overrides.lock.guard"),
        )
        .expect_err("truncated lock metadata should be reported as contention");
        let message = error.to_string();

        assert!(message.contains("locked by another process"));
        assert!(message.contains("lock metadata unreadable"));

        remove_dir_all(&dir).expect("failed to clean up temp test dir");
    }

    #[test]
    fn stale_legacy_lock_is_replaced_with_metadata_lock() {
        let dir = unique_test_dir();
        create_dir_all(&dir).expect("failed to create temp test dir");
        let path = dir.join("lqos_overrides.lock");
        write(&path, "2147483647\n").expect("failed to write stale temp lock");

        {
            let _lock = FileLock::new_at(
                "load effective overrides",
                &path,
                &dir,
                &dir,
                &dir.join("lqos_overrides.lock.guard"),
            )
            .expect("stale legacy lock should be replaceable");
            let contents = read_to_string(&path).expect("failed to read temp lock");

            assert!(contents.contains(&format!("pid={}", std::process::id())));
            assert!(contents.contains("operation=load effective overrides"));
        }

        remove_dir_all(&dir).expect("failed to clean up temp test dir");
    }

    #[test]
    fn stale_structured_lock_is_replaced_with_metadata_lock() {
        let dir = unique_test_dir();
        create_dir_all(&dir).expect("failed to create temp test dir");
        let path = dir.join("lqos_overrides.lock");
        write(
            &path,
            "pid=2147483647\nprocess=old-holder\noperation=save overrides\ncreated_unix=1800000000\n",
        )
        .expect("failed to write stale structured temp lock");

        {
            let _lock = FileLock::new_at(
                "load effective overrides",
                &path,
                &dir,
                &dir,
                &dir.join("lqos_overrides.lock.guard"),
            )
            .expect("stale structured lock should be replaceable");
            let contents = read_to_string(&path).expect("failed to read temp lock");

            assert!(contents.contains(&format!("pid={}", std::process::id())));
            assert!(contents.contains("operation=load effective overrides"));
        }

        remove_dir_all(&dir).expect("failed to clean up temp test dir");
    }
}
