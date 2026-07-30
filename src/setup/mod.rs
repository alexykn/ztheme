mod autosuggestions;
pub(crate) mod install;
mod syntax_highlighting;

use std::io;
use std::path::PathBuf;

use crate::gitstatus;

pub fn run(assume_yes: bool) -> io::Result<()> {
    if !gitstatus::ensure_installed(assume_yes)? {
        return Err(io::Error::other("gitstatusd installation declined"));
    }
    let syntax_installed = syntax_highlighting::ensure_installed(assume_yes)?;
    let autosuggestions_installed = autosuggestions::ensure_installed(assume_yes)?;
    println!("gitstatusd\t{}", gitstatus::managed_binary().display());
    if syntax_installed {
        println!(
            "zsh-syntax-highlighting\t{}",
            syntax_highlighting::managed_script().display()
        );
    } else {
        println!("zsh-syntax-highlighting\tnot installed (optional)");
    }
    if autosuggestions_installed {
        println!(
            "zsh-autosuggestions\t{}",
            autosuggestions::managed_script().display()
        );
    } else {
        println!("zsh-autosuggestions\tnot installed (optional)");
    }
    Ok(())
}

pub fn autosuggestions_script() -> PathBuf {
    autosuggestions::managed_script()
}

pub fn syntax_highlighting_script() -> PathBuf {
    syntax_highlighting::managed_script()
}
