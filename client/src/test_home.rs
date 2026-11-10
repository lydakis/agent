//! A home of their own for a test binary's tests.
//!
//! Much here finds a person's things under `$HOME`: the config, the env
//! file, Sonny's state, the sandbox's lock files. A test that reaches any
//! of it through the real variable passes or fails by what the person's
//! settings say, and writes into their home. A helper that takes its folder
//! as an argument keeps one test out; this keeps out the test nobody
//! thought to give one.
//!
//! [`test_home!`](crate::test_home), named once in a test binary, gives the
//! whole process a fresh, empty folder as `HOME` before any test runs, and
//! removes it when the process exits. It names the folder in
//! `AGENT_TEST_HOME`, and a process started with that set keeps the `HOME`
//! it was given: a helper a test starts from its own binary stays in the
//! home its test chose for it. Set it by hand (to anything) to run a tool
//! against the person's own home.
use std::path::PathBuf;
use std::sync::OnceLock;

/// The folder this process was given as its home, once [`enter`] ran.
static HOME: OnceLock<PathBuf> = OnceLock::new();

/// Names the test home to what a test starts, and so says it has one.
const MARK: &str = "AGENT_TEST_HOME";

/// Runs [`enter`] before `main`, so before any test's thread starts.
///
/// The one static it makes is an entry of the list of functions the
/// loader runs on start (`.init_array`, or Mach-O's `__mod_init_func`).
#[macro_export]
macro_rules! test_home {
    () => {
        #[used]
        #[cfg_attr(
            target_vendor = "apple",
            unsafe(link_section = "__DATA,__mod_init_func")
        )]
        #[cfg_attr(not(target_vendor = "apple"), unsafe(link_section = ".init_array"))]
        static TEST_HOME: extern "C" fn() = {
            extern "C" fn enter() {
                $crate::test_home::enter();
            }
            enter
        };
    };
}

/// Make this process's home a fresh folder under the temp folder, short
/// enough for a socket's path, and remove it at exit. Called by
/// [`test_home!`](crate::test_home) before `main`; a test never calls it.
pub fn enter() {
    if std::env::var_os(MARK).is_some() {
        return;
    }
    let home = std::env::temp_dir().join(format!("agent-test-home-{}", std::process::id()));
    // One a killed run of this pid left behind is not this run's.
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::create_dir_all(&home);
    // SAFETY: this runs before `main`, while the process has one thread,
    // so nothing reads the environment as it changes.
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var(MARK, &home);
    }
    if HOME.set(home).is_ok() {
        // SAFETY: `leave` is a plain function that takes nothing and
        // unwinds nowhere, which is all `atexit` asks of one.
        unsafe { libc::atexit(leave) };
    }
}

/// The home [`enter`] made, when this process has one.
pub fn made() -> Option<&'static PathBuf> {
    HOME.get()
}

extern "C" fn leave() {
    if let Some(home) = HOME.get() {
        let _ = std::fs::remove_dir_all(home);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_tests_home_is_a_folder_of_their_own() {
        let home = super::made().expect("this crate's tests name a test home");
        assert_eq!(std::env::var_os("HOME").as_deref(), Some(home.as_os_str()));
        assert!(home.starts_with(std::env::temp_dir()) && home.is_dir());
    }
}
