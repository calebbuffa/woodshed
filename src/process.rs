//! Running build commands so an interrupt actually stops the build.
//!
//! A native build spawns a tree of processes - CMake, then a generator, then
//! dozens of compilers. Rust's default `Command` gives the child its own
//! handling, so pressing Ctrl+C during a ten-minute build stops `cargo` while
//! leaving compilers running, saturating the machine and holding file locks
//! that make the next build fail.
//!
//! [`run`] puts the child in its own process group and installs a handler that
//! tears down the whole group.

use std::io;
use std::process::{Command, ExitStatus};
use std::sync::Once;
use std::sync::atomic::{AtomicU32, Ordering};

static INSTALL_HANDLER: Once = Once::new();
static ACTIVE_PID: AtomicU32 = AtomicU32::new(0);

/// Run `command` to completion, ensuring Ctrl+C terminates its descendants.
///
/// `description` appears in the progress line printed before the child starts.
pub fn run(command: &mut Command, description: &str) -> io::Result<ExitStatus> {
    install_interrupt_handler();

    #[cfg(unix)]
    configure_unix_process_group(command);
    #[cfg(windows)]
    configure_windows_process_group(command);

    eprintln!("woodshed: starting {description}");
    let mut child = command.spawn()?;
    ACTIVE_PID.store(child.id(), Ordering::SeqCst);
    let result = child.wait();
    ACTIVE_PID.store(0, Ordering::SeqCst);
    result
}

/// Run `command`, turning a non-zero exit into a [`crate::Error::Tool`].
pub fn check(command: &mut Command, description: &str) -> crate::Result<()> {
    let status = run(command, description).map_err(|error| crate::Error::Tool {
        what: description.to_string(),
        detail: format!("could not start: {error}"),
    })?;

    if status.success() {
        Ok(())
    } else {
        Err(crate::Error::Tool {
            what: description.to_string(),
            detail: format!("exited with {status}"),
        })
    }
}

fn install_interrupt_handler() {
    INSTALL_HANDLER.call_once(|| {
        let installed = ctrlc::set_handler(|| {
            let pid = ACTIVE_PID.load(Ordering::SeqCst);
            if pid == 0 {
                return;
            }

            #[cfg(unix)]
            {
                // The child leads its process group, so a negative PID reaches
                // the build tool and every descendant it spawned.
                unsafe {
                    libc::kill(-(pid as libc::pid_t), libc::SIGTERM);
                }
            }

            #[cfg(windows)]
            {
                // `/T` reaches the whole tree; `/F` is required because build
                // tools routinely ignore Ctrl+C.
                let _ = std::process::Command::new("taskkill")
                    .args(["/PID", &pid.to_string(), "/T", "/F"])
                    .status();
            }
        });

        // A build that cannot be interrupted cleanly is worse than one that
        // prints a warning, so this is not fatal.
        if let Err(error) = installed {
            eprintln!("woodshed: could not install Ctrl+C handler ({error}); interrupting a build may leave compilers running");
        }
    });
}

#[cfg(unix)]
fn configure_unix_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(windows)]
fn configure_windows_process_group(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP);
}
