//! Compile-time static Platform Abstraction Layer (PAL) facade for Loom.
//!
//! Exports [`NativeBackend`] via `#[cfg(target_os)]` type aliases and provides
//! lightweight cross-platform desktop lifecycle helpers such as
//! [`SingleInstanceGuard`] so upper layers remain free of OS-specific
//! conditional compilation and dynamic trait-object overhead.

#![warn(missing_docs)]

pub use loom_pal_api::*;

/// Active compile-time native backend on Windows.
#[cfg(windows)]
pub type NativeBackend = loom_pal_windows::WindowsBackend;

/// Active compile-time native backend on macOS.
#[cfg(target_os = "macos")]
pub type NativeBackend = loom_pal_macos::MacosBackend;

/// Active compile-time native backend on Linux.
#[cfg(target_os = "linux")]
pub type NativeBackend = loom_pal_linux::LinuxBackend;

/// Cross-platform single-instance application lock guard.
///
/// Wraps OS-native named mutex / domain-socket / flock primitives via `single-instance`
/// so `loom-app` can detect an already-running Loom instance without platform `#[cfg]` code.
/// The underlying OS lock is automatically released when this guard is dropped.
pub struct SingleInstanceGuard {
    inner: single_instance::SingleInstance,
}

impl std::fmt::Debug for SingleInstanceGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SingleInstanceGuard")
            .field("is_single", &self.is_single())
            .finish()
    }
}

impl SingleInstanceGuard {
    /// Attempts to acquire the OS-level single-instance lock identified by `app_id`.
    ///
    /// Call [`Self::is_single`] on the returned guard to determine whether this
    /// process is the primary instance (`true`) or a duplicate launch (`false`).
    pub fn acquire(app_id: &str) -> Result<Self, PalError> {
        if app_id.trim().is_empty() {
            return Err(PalError::InvalidArgument(
                "single-instance app_id cannot be empty".into(),
            ));
        }

        let inner =
            single_instance::SingleInstance::new(app_id).map_err(|err| PalError::OsError {
                code: -1,
                message: format!("failed to create single-instance lock '{app_id}': {err}"),
            })?;

        Ok(Self { inner })
    }

    /// Returns `true` if this guard owns the primary single-instance lock.
    #[inline]
    pub fn is_single(&self) -> bool {
        self.inner.is_single()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_instance_guard_detects_duplicate_and_releases_on_drop() {
        let unique_id = format!("loom-pal-single-instance-test-{}", std::process::id());

        {
            let primary = SingleInstanceGuard::acquire(&unique_id).expect("acquire primary");
            assert!(primary.is_single());

            let secondary = SingleInstanceGuard::acquire(&unique_id).expect("acquire secondary");
            assert!(!secondary.is_single());
        }

        // After dropping both guards, a fresh acquisition becomes the primary instance again.
        let reacquired = SingleInstanceGuard::acquire(&unique_id).expect("reacquire after drop");
        assert!(reacquired.is_single());
    }

    #[test]
    fn single_instance_guard_rejects_empty_id() {
        let err = SingleInstanceGuard::acquire("   ").expect_err("empty app_id must error");
        assert!(matches!(err, PalError::InvalidArgument(_)));
    }
}
