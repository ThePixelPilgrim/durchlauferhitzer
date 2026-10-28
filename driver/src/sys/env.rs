//! Removing environment variables (A7: the systemd variables are cleared
//! after use), which Rust 2024 marks `unsafe` because another thread may
//! read the environment at the same time.

use std::io;

/// Removes `names` from the environment, but only while this is the
/// process's only thread (counted in `/proc/self/task`): with no other
/// thread nothing can read the environment concurrently, and only this
/// thread could start one. More than one thread, or a `/proc` that cannot
/// be read, is an error and nothing is removed.
pub fn remove_vars(names: &[&str]) -> io::Result<()> {
    let threads = std::fs::read_dir("/proc/self/task")?.count();
    if threads != 1 {
        return Err(io::Error::other(format!(
            "{threads} threads run: the environment can only be changed by the only thread"
        )));
    }
    for name in names {
        // SAFETY: this is the only thread of the process (checked above,
        // and only it could have started another since), so no other
        // thread reads or writes the environment meanwhile.
        unsafe { std::env::remove_var(name) };
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;

    use super::remove_vars;

    /// The guard: with a second thread alive nothing is removed.
    #[test]
    fn refused_while_another_thread_runs() {
        let (stop, wait) = mpsc::channel::<()>();
        let other = thread::spawn(move || {
            let _ = wait.recv();
        });
        let name = "DURCHLAUFERHITZER_ENV_TEST_NEVER_SET";
        let Err(err) = remove_vars(&[name]) else {
            panic!("removed with another thread running");
        };
        assert!(err.to_string().contains("threads run"), "{err}");
        drop(stop);
        other.join().unwrap();
    }
}
