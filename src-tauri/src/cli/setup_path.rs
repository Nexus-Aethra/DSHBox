//! `dshbox setup-path` — add the dshbox install directory to the
//! user's PATH so subsequent shells and agent-runner subprocesses
//! can resolve `dshbox` directly.
//!
//! Why this command exists: the Windows NSIS installer used to ship
//! the binary into `C:\Program Files\DSH Box\` without writing that
//! directory to PATH. Linux and macOS installs go through `/usr/bin`
//! where the directory is already on PATH, so on those platforms the
//! command is a no-op that prints a one-liner.

use box_runtime::{add_to_user_path, self_install_directory};

pub(crate) fn command(_arguments: &[String]) -> Result<(), String> {
    let directory = self_install_directory()
        .ok_or_else(|| "cannot determine dshbox install directory".to_owned())?;
    if !directory.is_dir() {
        return Err(format!(
            "install directory does not exist: {}",
            directory.display()
        ));
    }
    add_to_user_path(&directory)?;
    #[cfg(target_os = "windows")]
    {
        println!(
            "added {} to HKCU\\Environment\\Path",
            directory.display()
        );
        println!("open a new terminal (or sign out + back in) for the change to take effect.");
    }
    #[cfg(not(target_os = "windows"))]
    {
        println!("appended {} to your shell rc file", directory.display());
        println!("restart the shell or `source` the rc to pick up the change.");
    }
    Ok(())
}
