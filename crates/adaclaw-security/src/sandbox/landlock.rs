//! Linux Landlock LSM — fine-grained, process-level file-system access control.
//!
//! On Linux with kernel ≥ 5.13, this applies Landlock rules that restrict which
//! file-system paths the AdaClaw process can access, providing a second layer of
//! defense beyond the workspace path check in `sandbox/workspace.rs`.
//!
//! On Windows and macOS (or Linux kernels < 5.13), this is a **no-op** — the
//! function succeeds and logs a debug message. The workspace path sandbox still
//! applies.
//!
//! # Design
//!
//! Unlike `seccomp` (which filters syscalls), Landlock filters at the VFS level:
//! you grant access to specific directory trees, and the kernel denies everything
//! else. This is ideal for confining shell/file tool execution to the workspace.
//!
//! # Usage
//!
//! ```rust,ignore
//! use adaclaw_security::sandbox::landlock::{LandlockConfig, apply};
//! let config = LandlockConfig::workspace_only("/app/workspace");
//! if apply(&config).is_err() {
//!     // Landlock failed — log but don't abort (workspace sandbox is still active)
//! }
//! ```
//!
//! Reference: <https://www.kernel.org/doc/html/latest/userspace-api/landlock.html>

use tracing::debug;
#[allow(unused_imports)]
use tracing::warn;

// ── LandlockConfig ────────────────────────────────────────────────────────────

/// Describes which paths the process is allowed to access after Landlock is applied.
#[derive(Debug, Clone)]
pub struct LandlockConfig {
    /// Paths the process may read (and list directories).
    pub read_paths: Vec<String>,
    /// Paths the process may read AND write (includes create/delete within them).
    pub write_paths: Vec<String>,
    /// Paths the process may execute files from.
    pub exec_paths: Vec<String>,
}

impl LandlockConfig {
    /// Convenience: restrict to a single workspace directory (read + write).
    /// Adds `/tmp` and `/proc/self` for normal process operation.
    pub fn workspace_only(workspace: &str) -> Self {
        Self {
            read_paths: vec![
                workspace.to_string(),
                "/tmp".to_string(),
                "/proc/self".to_string(),
                "/dev/null".to_string(),
                "/dev/urandom".to_string(),
            ],
            write_paths: vec![workspace.to_string(), "/tmp".to_string()],
            exec_paths: vec![],
        }
    }

    /// Allow reading from an additional path (e.g. system libraries needed at runtime).
    pub fn with_read(mut self, path: impl Into<String>) -> Self {
        self.read_paths.push(path.into());
        self
    }

    /// Allow reading and writing to an additional path.
    pub fn with_write(mut self, path: impl Into<String>) -> Self {
        let p = path.into();
        self.read_paths.push(p.clone());
        self.write_paths.push(p);
        self
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Check whether the current kernel supports Landlock.
///
/// Returns `true` only on Linux kernels that expose the Landlock ABI (≥ 5.13).
/// Always returns `false` on Windows and macOS.
pub fn is_supported() -> bool {
    #[cfg(target_os = "linux")]
    {
        linux_impl::check_support()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Apply Landlock restrictions to the current process.
///
/// - On **Linux** (kernel ≥ 5.13): applies the configured path rules.
///   If the kernel does not support Landlock, gracefully degrades to a no-op.
/// - On **Windows / macOS**: always succeeds as a no-op (workspace sandbox applies).
///
/// # Errors
///
/// Returns `Err` only if applying `PR_SET_NO_NEW_PRIVS` fails — a fatal
/// kernel operation that should never fail in practice.
pub fn apply(config: &LandlockConfig) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        linux_impl::apply_impl(config)
    }
    #[cfg(not(target_os = "linux"))]
    {
        debug!("Landlock: not available on this platform (no-op)");
        let _ = config;
        Ok(())
    }
}

// ── Linux implementation ──────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
#[allow(unsafe_code)] // landlock requires raw libc syscalls — intentional unsafe
mod linux_impl {
    use super::*;
    use std::ffi::CString;
    use std::os::unix::io::RawFd;

    // ── Landlock ABI constants (from uapi/linux/landlock.h) ──────────────────

    const SYS_LANDLOCK_CREATE_RULESET: i64 = 444;
    const SYS_LANDLOCK_ADD_RULE: i64 = 445;
    const SYS_LANDLOCK_RESTRICT_SELF: i64 = 446;
    const LANDLOCK_RULE_PATH_BENEATH: u32 = 1;

    // Filesystem access rights
    const LANDLOCK_ACCESS_FS_EXECUTE: u64 = 1 << 0;
    const LANDLOCK_ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
    const LANDLOCK_ACCESS_FS_READ_FILE: u64 = 1 << 2;
    const LANDLOCK_ACCESS_FS_READ_DIR: u64 = 1 << 3;
    const LANDLOCK_ACCESS_FS_REMOVE_DIR: u64 = 1 << 4;
    const LANDLOCK_ACCESS_FS_REMOVE_FILE: u64 = 1 << 5;
    const LANDLOCK_ACCESS_FS_MAKE_CHAR: u64 = 1 << 6;
    const LANDLOCK_ACCESS_FS_MAKE_DIR: u64 = 1 << 7;
    const LANDLOCK_ACCESS_FS_MAKE_REG: u64 = 1 << 8;
    const LANDLOCK_ACCESS_FS_MAKE_SOCK: u64 = 1 << 9;
    const LANDLOCK_ACCESS_FS_MAKE_FIFO: u64 = 1 << 10;
    const LANDLOCK_ACCESS_FS_MAKE_BLOCK: u64 = 1 << 11;
    const LANDLOCK_ACCESS_FS_MAKE_SYM: u64 = 1 << 12;

    const ALL_FS_ACCESS: u64 = LANDLOCK_ACCESS_FS_EXECUTE
        | LANDLOCK_ACCESS_FS_WRITE_FILE
        | LANDLOCK_ACCESS_FS_READ_FILE
        | LANDLOCK_ACCESS_FS_READ_DIR
        | LANDLOCK_ACCESS_FS_REMOVE_DIR
        | LANDLOCK_ACCESS_FS_REMOVE_FILE
        | LANDLOCK_ACCESS_FS_MAKE_CHAR
        | LANDLOCK_ACCESS_FS_MAKE_DIR
        | LANDLOCK_ACCESS_FS_MAKE_REG
        | LANDLOCK_ACCESS_FS_MAKE_SOCK
        | LANDLOCK_ACCESS_FS_MAKE_FIFO
        | LANDLOCK_ACCESS_FS_MAKE_BLOCK
        | LANDLOCK_ACCESS_FS_MAKE_SYM;

    const READ_ACCESS: u64 = LANDLOCK_ACCESS_FS_READ_FILE | LANDLOCK_ACCESS_FS_READ_DIR;

    const WRITE_ACCESS: u64 = LANDLOCK_ACCESS_FS_WRITE_FILE
        | LANDLOCK_ACCESS_FS_MAKE_REG
        | LANDLOCK_ACCESS_FS_MAKE_DIR
        | LANDLOCK_ACCESS_FS_REMOVE_FILE
        | LANDLOCK_ACCESS_FS_REMOVE_DIR;

    const EXEC_ACCESS: u64 = LANDLOCK_ACCESS_FS_EXECUTE;

    // ── ABI structs ───────────────────────────────────────────────────────────

    #[repr(C)]
    struct LandlockRulesetAttr {
        handled_access_fs: u64,
    }

    #[repr(C)]
    struct LandlockPathBeneathAttr {
        allowed_access: u64,
        parent_fd: i32,
    }

    // ── Public functions ──────────────────────────────────────────────────────

    /// Check if Landlock is supported by attempting to create a minimal ruleset.
    pub fn check_support() -> bool {
        let attr = LandlockRulesetAttr {
            handled_access_fs: ALL_FS_ACCESS,
        };
        let ret = unsafe {
            libc::syscall(
                SYS_LANDLOCK_CREATE_RULESET,
                &attr as *const _,
                std::mem::size_of::<LandlockRulesetAttr>() as libc::size_t,
                0u32,
            )
        };
        if ret >= 0 {
            unsafe { libc::close(ret as i32) };
            true
        } else {
            false
        }
    }

    /// Apply Landlock restrictions.
    pub fn apply_impl(config: &super::LandlockConfig) -> anyhow::Result<()> {
        // Create the ruleset
        let attr = LandlockRulesetAttr {
            handled_access_fs: ALL_FS_ACCESS,
        };
        let ruleset_fd = unsafe {
            libc::syscall(
                SYS_LANDLOCK_CREATE_RULESET,
                &attr as *const _,
                std::mem::size_of::<LandlockRulesetAttr>() as libc::size_t,
                0u32,
            )
        };

        if ruleset_fd < 0 {
            let err = std::io::Error::last_os_error();
            warn!(
                error = %err,
                "Landlock not supported on this kernel — gracefully degrading to workspace sandbox only"
            );
            return Ok(()); // Graceful degradation
        }

        let ruleset_fd = ruleset_fd as RawFd;

        // Add read-only paths
        for path in &config.read_paths {
            if let Err(e) = add_path_rule(ruleset_fd, path, READ_ACCESS) {
                warn!(path, error = %e, "Failed to add Landlock read rule");
            }
        }

        // Add read-write paths
        for path in &config.write_paths {
            if let Err(e) = add_path_rule(ruleset_fd, path, READ_ACCESS | WRITE_ACCESS) {
                warn!(path, error = %e, "Failed to add Landlock write rule");
            }
        }

        // Add executable paths
        for path in &config.exec_paths {
            if let Err(e) = add_path_rule(ruleset_fd, path, READ_ACCESS | EXEC_ACCESS) {
                warn!(path, error = %e, "Failed to add Landlock exec rule");
            }
        }

        // Enable PR_SET_NO_NEW_PRIVS (required before restricting self)
        let ret = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1usize, 0usize, 0usize, 0usize) };
        if ret < 0 {
            unsafe { libc::close(ruleset_fd) };
            anyhow::bail!(
                "prctl PR_SET_NO_NEW_PRIVS failed: {}",
                std::io::Error::last_os_error()
            );
        }

        // Apply the ruleset to the current thread (and all future threads/children)
        let ret = unsafe { libc::syscall(SYS_LANDLOCK_RESTRICT_SELF, ruleset_fd, 0u32) };
        unsafe { libc::close(ruleset_fd) };

        if ret < 0 {
            anyhow::bail!(
                "landlock_restrict_self failed: {}",
                std::io::Error::last_os_error()
            );
        }

        debug!(
            read_paths = ?config.read_paths,
            write_paths = ?config.write_paths,
            "Landlock restrictions applied successfully"
        );

        Ok(())
    }

    /// Add a path-beneath rule to the ruleset.
    fn add_path_rule(ruleset_fd: RawFd, path: &str, allowed_access: u64) -> anyhow::Result<()> {
        let c_path =
            CString::new(path).map_err(|_| anyhow::anyhow!("Path contains null byte: {}", path))?;

        let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };

        if fd < 0 {
            // Path doesn't exist on this system — skip gracefully
            debug!(path, "Landlock: path does not exist, skipping rule");
            return Ok(());
        }

        let rule = LandlockPathBeneathAttr {
            allowed_access,
            parent_fd: fd,
        };

        let ret = unsafe {
            libc::syscall(
                SYS_LANDLOCK_ADD_RULE,
                ruleset_fd,
                LANDLOCK_RULE_PATH_BENEATH,
                &rule as *const _,
                0u32,
            )
        };

        unsafe { libc::close(fd) };

        if ret < 0 {
            warn!(
                path,
                error = %std::io::Error::last_os_error(),
                "Failed to add Landlock rule"
            );
        }

        Ok(())
    }
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_supported_no_panic() {
        // Just verify it doesn't panic — result is platform-dependent
        let _ = is_supported();
    }

    #[test]
    fn test_apply_noop_on_non_linux() {
        // On non-Linux, apply should always succeed
        #[cfg(not(target_os = "linux"))]
        {
            let config = LandlockConfig::workspace_only("/tmp/test");
            assert!(apply(&config).is_ok());
        }
    }

    #[test]
    fn test_workspace_only_config() {
        let config = LandlockConfig::workspace_only("/app/workspace");
        assert!(config.write_paths.contains(&"/app/workspace".to_string()));
        assert!(config.read_paths.contains(&"/app/workspace".to_string()));
        assert!(config.read_paths.contains(&"/tmp".to_string()));
    }

    #[test]
    fn test_config_builder() {
        let config = LandlockConfig::workspace_only("/workspace")
            .with_read("/usr/lib")
            .with_write("/tmp/extra");
        assert!(config.read_paths.contains(&"/usr/lib".to_string()));
        assert!(config.write_paths.contains(&"/tmp/extra".to_string()));
        assert!(config.read_paths.contains(&"/tmp/extra".to_string()));
    }
}
