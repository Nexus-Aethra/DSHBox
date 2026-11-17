use std::process::Command;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// Apply platform flags for a non-interactive child. Windows children never
/// create a console window; detached children also receive their own group.
pub fn configure_non_interactive(command: &mut Command, new_process_group: bool) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut flags = CREATE_NO_WINDOW;
        if new_process_group { flags |= CREATE_NEW_PROCESS_GROUP; }
        command.creation_flags(flags);
    }
    #[cfg(unix)]
    {
        if new_process_group {
            use std::os::unix::process::CommandExt;
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1 { return Err(std::io::Error::last_os_error()); }
                    Ok(())
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configure_is_safe_to_call() {
        let mut command = Command::new("true");
        configure_non_interactive(&mut command, false);
    }
}
