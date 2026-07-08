//! OS-level sandbox for `run_command` (Linux landlock).
//!
//! Defense-in-depth UNDER the path-jail: even if the app-level jail (or a backend command
//! policy) is bypassed, the kernel confines what a spawned shell command can WRITE. We
//! restrict only the WRITE family of filesystem access rights — reads and execs stay
//! unrestricted, so the dev toolchain (compilers, package managers reading `~/.cargo`,
//! `~/.gitconfig`, system libs) keeps working — while writes are allowed ONLY under the
//! workspace, the standard build caches, and the temp dirs. So `rm -rf ~/Documents`,
//! clobbering `~/.bashrc`, or writing system files is blocked; building the project is not.
//!
//! Opt-in (`config.sandbox`), Linux-only, and FAIL-SAFE: if landlock is unavailable or the
//! ruleset can't be built we run WITHOUT it (the path-jail still applies) rather than break
//! the agent. The ruleset is built in the parent (cheap, a few syscalls) and only
//! `restrict_self()` runs in the post-fork child via `pre_exec`.

use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Enable/disable the command sandbox process-wide (set once from config at startup).
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub use imp::{restrict_current, ruleset_for, supported};

#[cfg(target_os = "linux")]
mod imp {
    use std::path::{Path, PathBuf};

    use landlock::{
        AccessFs, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset as LandlockRuleset,
        RulesetAttr, RulesetCreated, RulesetCreatedAttr, ABI,
    };

    /// The built, not-yet-applied ruleset handed to the post-fork child. Opaque to callers.
    pub type Ruleset = RulesetCreated;

    /// Standard writable build-cache dirs under $HOME (tools insist on writing here). Only the
    /// ones that already exist are granted (landlock can't add a rule for a missing path).
    const HOME_CACHE_DIRS: &[&str] = &[
        ".cache",
        ".cargo",
        ".rustup",
        ".npm",
        ".config",
        ".local",
        ".gradle",
        ".m2",
        ".gem",
        "go",
        ".go",
        ".deno",
        ".bun",
        ".yarn",
        ".pnpm-store",
        ".cabal",
        ".stack",
        ".nuget",
        ".dotnet",
        ".composer",
        ".ivy2",
        ".sbt",
        ".node-gyp",
    ];

    fn write_targets(workspace_root: &Path) -> Vec<PathBuf> {
        let mut out = vec![workspace_root.to_path_buf()];
        for p in ["/tmp", "/var/tmp", "/dev/null", "/dev/tty", "/dev/shm"] {
            out.push(PathBuf::from(p));
        }
        if let Some(home) = dirs::home_dir() {
            for d in HOME_CACHE_DIRS {
                out.push(home.join(d));
            }
        }
        out.retain(|p| p.exists());
        out
    }

    fn build(workspace_root: &Path) -> Option<RulesetCreated> {
        let abi = ABI::V5; // BestEffort below degrades on older kernels (down to V1)
        let write = AccessFs::from_write(abi);
        let mut ruleset = LandlockRuleset::default()
            .set_compatibility(CompatLevel::BestEffort)
            .handle_access(write)
            .ok()?
            .create()
            .ok()?;
        let mut granted = 0usize;
        for dir in write_targets(workspace_root) {
            let Ok(fd) = PathFd::new(&dir) else { continue };
            // add_rule CONSUMES the ruleset; reassign in both arms. A genuine error (pre-
            // filtered paths shouldn't error) → fail-safe to no sandbox rather than a
            // half-built ruleset that could block the workspace too.
            ruleset = match ruleset.add_rule(PathBeneath::new(fd, write)) {
                Ok(next) => {
                    granted += 1;
                    next
                }
                Err(_) => return None,
            };
        }
        // No grantable path (not even the workspace) → applying the ruleset would block ALL
        // writes incl. the workspace; safer to skip than to break every command.
        if granted == 0 {
            return None;
        }
        Some(ruleset)
    }

    /// Build the per-command write-confinement ruleset when the sandbox is enabled, else None.
    pub fn ruleset_for(workspace_root: &Path) -> Option<Ruleset> {
        if !super::enabled() {
            return None;
        }
        build(workspace_root)
    }

    /// Apply a built ruleset to the CURRENT process (called in the post-fork child, pre-exec).
    pub fn restrict_current(ruleset: Ruleset) -> std::io::Result<()> {
        ruleset
            .restrict_self()
            .map(|_| ())
            .map_err(|e| std::io::Error::other(format!("landlock restrict_self: {e}")))
    }

    /// True if the running kernel offers landlock at all (for a one-time startup warning).
    pub fn supported() -> bool {
        LandlockRuleset::default()
            .set_compatibility(CompatLevel::BestEffort)
            .handle_access(AccessFs::from_write(ABI::V1))
            .and_then(|r| r.create())
            .is_ok()
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use std::path::Path;

    /// No-op placeholder on non-Linux (the sandbox is Linux-only).
    pub struct Ruleset;

    pub fn ruleset_for(_workspace_root: &Path) -> Option<Ruleset> {
        None
    }

    pub fn restrict_current(_ruleset: Ruleset) -> std::io::Result<()> {
        Ok(())
    }

    pub fn supported() -> bool {
        false
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn landlock_blocks_writes_outside_grants() {
        use std::os::unix::process::CommandExt;

        set_enabled(true);
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_path_buf();
        let built = ruleset_for(&ws);
        set_enabled(false); // minimise the global-on window (the ruleset is already built)
        let Some(rs) = built else {
            eprintln!("landlock unavailable on this kernel — skipping");
            return;
        };

        let inside = ws.join("inside.txt");
        // A path under NO granted tree (root fs; not the workspace, /tmp, or a cache).
        let outside = std::path::Path::new("/landlock_probe_outside_xyz.txt");
        let _ = std::fs::remove_file(outside);
        let script = format!(
            "echo ok > '{}'; (echo bad > '{}') 2>/dev/null && echo WROTE_OUTSIDE || echo BLOCKED",
            inside.display(),
            outside.display(),
        );
        let mut rs = Some(rs);
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(&script);
        // SAFETY: applies the prebuilt landlock ruleset in the forked child before exec.
        unsafe {
            cmd.pre_exec(move || match rs.take() {
                Some(r) => restrict_current(r),
                None => Ok(()),
            });
        }
        let out = cmd.output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);

        assert!(inside.exists(), "write INSIDE the workspace must succeed");
        assert!(
            stdout.contains("BLOCKED"),
            "write OUTSIDE must be blocked, got: {stdout:?}"
        );
        assert!(!outside.exists(), "the out-of-jail file must not exist");
    }
}
