//! Low-level fd/FFI plumbing for the fleet adapter: swap this process's
//! stdout onto /dev/null for one action ([`suppress_stdout`]) without leaking
//! the saved descriptor into detached children ([`dup_cloexec`]). Unix-only;
//! the non-unix build is a pass-through.
//!
//! CAVEAT: fd 1 is PROCESS-GLOBAL, so while a suppression window is open,
//! stdout is /dev/null for EVERY thread — not just the one that called
//! [`suppress_stdout`]. A concurrent thread's println/event lands in the
//! void for the duration. Acceptable today because the windows are brief and
//! the callers (fleet verbs) don't overlap chatty threads; revisit before
//! wrapping anything long-running in a suppression window.

/// Run `f` with this process's stdout (fd 1) redirected to /dev/null, then
/// restore it. Used to swallow babysit's parent-path banner while keeping
/// looop's own output. Unix-only; a no-op redirect failure just runs `f`.
///
/// The `saved` copy of fd 1 is created close-on-exec: `f` spawns the detached
/// worker, so a plain `dup(1)` would leak our stdout into that child. When the
/// caller captured our stdout through a pipe (`$(looop worker start …)`), a
/// leaked write end keeps that pipe open for the worker's entire lifetime, so
/// the caller's read never sees EOF and the command *looks* hung even though we
/// returned immediately. `dup_cloexec` keeps the copy out of the child.
#[cfg(unix)]
pub(crate) fn suppress_stdout<T>(f: impl FnOnce() -> T) -> T {
    use std::cell::Cell;
    use std::io::Write;
    use std::os::unix::io::AsRawFd;
    unsafe extern "C" {
        fn dup2(a: i32, b: i32) -> i32;
    }
    if SUPPRESS_DEPTH.with(Cell::get) > 0 {
        return f();
    }
    let Ok(devnull) = std::fs::OpenOptions::new().write(true).open("/dev/null") else {
        return f();
    };
    let _swap = STDOUT_SWAP
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _ = std::io::stdout().flush();
    // Close-on-exec so the detached worker `f` spawns never inherits this
    // copy of our stdout (see the doc comment above).
    let saved = unsafe { dup_cloexec(1) };
    if saved < 0 {
        return f();
    }

    /// RAII restore: puts the saved fd back on 1 and decrements the depth on
    /// DROP, so even a PANICKING action unwinds with stdout restored — without
    /// this, a panic inside `f` would leave the whole process silenced on
    /// /dev/null (and the depth counter stuck). Declared AFTER `_swap`, so it
    /// drops FIRST: fd 1 is restored while the swap lock is still held.
    struct Restore(i32);
    impl Drop for Restore {
        fn drop(&mut self) {
            unsafe extern "C" {
                fn dup2(a: i32, b: i32) -> i32;
                fn close(fd: i32) -> i32;
            }
            let _ = std::io::Write::flush(&mut std::io::stdout());
            unsafe {
                dup2(self.0, 1);
                close(self.0);
            }
            SUPPRESS_DEPTH.with(|d| d.set(d.get() - 1));
        }
    }

    unsafe { dup2(devnull.as_raw_fd(), 1) };
    SUPPRESS_DEPTH.with(|d| d.set(d.get() + 1));
    let _restore = Restore(saved);
    f()
}

/// fd 1 is PROCESS-GLOBAL state: two threads swapping it concurrently could
/// each "save" the other's /dev/null and restore THAT as the real stdout,
/// leaving the whole process silenced forever (observed as parallel tests
/// losing the libtest results/summary mid-run). This mutex serializes the
/// whole swap window; [`SUPPRESS_DEPTH`] makes NESTED suppression on the same
/// thread (execute → start_worker → suppress_stdout) a plain pass-through
/// instead of a self-deadlock — fd 1 is already /dev/null inside the outer
/// window, so the inner swap was always redundant.
#[cfg(unix)]
static STDOUT_SWAP: std::sync::Mutex<()> = std::sync::Mutex::new(());
#[cfg(unix)]
thread_local! {
    static SUPPRESS_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// `dup(fd)` that returns a close-on-exec copy, so it is not inherited across a
/// later `exec`/spawn. Returns a negative value on failure (caller falls back).
///
/// Hand-written FFI is deliberate: replacing it would require the `libc` (or
/// `nix`) crate as a new dependency, which this project avoids. The constants
/// are safe to hardcode — F_SETFD / FD_CLOEXEC are POSIX-stable and are 2 / 1
/// on both macOS and Linux (verified by the `dup_cloexec_sets_close_on_exec`
/// test, which round-trips through F_GETFD).
#[cfg(unix)]
unsafe fn dup_cloexec(fd: i32) -> i32 {
    unsafe extern "C" {
        fn dup(fd: i32) -> i32;
        fn fcntl(fd: i32, cmd: i32, ...) -> i32;
        fn close(fd: i32) -> i32;
    }
    const F_SETFD: i32 = 2;
    const FD_CLOEXEC: i32 = 1;
    unsafe {
        let copy = dup(fd);
        if copy < 0 {
            return copy;
        }
        // If we can't set close-on-exec the copy would leak into the detached
        // worker (the very leak this guards against), so treat it as a hard
        // failure: close the copy and return a negative fd so the caller falls
        // back safely instead of running with a non-CLOEXEC descriptor.
        if fcntl(copy, F_SETFD, FD_CLOEXEC) < 0 {
            close(copy);
            return -1;
        }
        copy
    }
}

#[cfg(not(unix))]
pub(crate) fn suppress_stdout<T>(f: impl FnOnce() -> T) -> T {
    f()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Regression: suppress_stdout swaps fd 1 — PROCESS-GLOBAL state. Without
    // serialization, two concurrent suppressions could each "save" the other's
    // /dev/null and restore it as the real stdout, permanently silencing the
    // process (this ate the libtest results/summary under parallel `cargo
    // test`). Also asserts nesting on one thread passes through (no deadlock).
    #[cfg(unix)]
    #[test]
    fn suppress_stdout_is_reentrant_and_race_free() {
        let before = stdout_ident();
        // Nested on one thread: pass-through, not a self-deadlock.
        assert_eq!(suppress_stdout(|| suppress_stdout(|| 42)), 42);
        // Hammer the swap from two threads at once.
        std::thread::scope(|s| {
            for _ in 0..2 {
                s.spawn(|| {
                    for _ in 0..200 {
                        suppress_stdout(|| std::hint::black_box(()));
                    }
                });
            }
        });
        assert_eq!(
            stdout_ident(),
            before,
            "stdout identity must survive concurrent suppression windows"
        );
    }

    /// Observe fd 1 while HOLDING the swap lock: another parallel test may
    /// legitimately be inside its own suppression window right now, and an
    /// unsynchronized peek would see its temporary /dev/null.
    #[cfg(unix)]
    fn stdout_ident() -> (u64, u64, u64) {
        use std::os::fd::BorrowedFd;
        use std::os::unix::fs::MetadataExt;
        let _swap = STDOUT_SWAP
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let fd = unsafe { BorrowedFd::borrow_raw(1) };
        let f = std::fs::File::from(fd.try_clone_to_owned().unwrap());
        let m = f.metadata().unwrap();
        (m.dev(), m.ino(), m.rdev())
    }

    // Regression: a PANICKING action must not leave the process silenced — the
    // RAII guard restores fd 1 and the depth counter during unwinding.
    #[cfg(unix)]
    #[test]
    fn suppress_stdout_restores_fd1_when_the_action_panics() {
        let before = stdout_ident();
        let unwound = std::panic::catch_unwind(|| suppress_stdout(|| panic!("boom")));
        assert!(unwound.is_err(), "the panic propagates");
        assert_eq!(
            stdout_ident(),
            before,
            "fd 1 restored during unwinding — the process is not left on /dev/null"
        );
        // Depth was decremented too: a later suppression still round-trips.
        assert_eq!(suppress_stdout(|| 7), 7);
        assert_eq!(stdout_ident(), before);
    }

    // Regression: the fd we dup inside suppress_stdout MUST be close-on-exec.
    // A plain dup(1) leaked our stdout into the detached worker; when the
    // caller captured our stdout via a pipe (`out=$(looop worker start …)`)
    // that leaked write end kept the pipe open for the worker's whole lifetime,
    // so the caller never saw EOF and `worker start` looked hung. Assert the
    // copy carries FD_CLOEXEC so no spawned child can inherit it.
    #[cfg(unix)]
    #[test]
    fn dup_cloexec_sets_close_on_exec() {
        unsafe extern "C" {
            fn fcntl(fd: i32, cmd: i32, ...) -> i32;
            fn close(fd: i32) -> i32;
        }
        const F_GETFD: i32 = 1;
        const FD_CLOEXEC: i32 = 1;
        unsafe {
            // dup fd 2 (stderr): always open under the test harness, and we
            // never touch its target so this stays side-effect free.
            let copy = dup_cloexec(2);
            assert!(copy >= 0, "dup_cloexec failed");
            let flags = fcntl(copy, F_GETFD);
            close(copy);
            assert!(flags >= 0, "F_GETFD failed");
            assert_eq!(
                flags & FD_CLOEXEC,
                FD_CLOEXEC,
                "dup_cloexec copy must be close-on-exec so detached workers don't \
                 inherit (and pin open) the caller's captured stdout pipe"
            );
        }
    }
}
