//! Takes a wheel and installs it, either in a venv or for monotrail

// The pub ones are reused by monotrail
pub use install_location::{normalize_name, InstallLocation, LockedDir};
use platform_info::PlatformInfoError;
use std::io;
use std::path::Path;
use thiserror::Error;
pub use wheel::{
    get_script_launcher, install_wheel, parse_key_value_file, read_record_file, relative_to,
    Script, MONOTRAIL_SCRIPT_SHEBANG,
};
pub use wheel_tags::{Arch, CompatibleTags, Os, WheelFilename};
use zip::result::ZipError;

mod install_location;
#[cfg(feature = "python_bindings")]
mod python_bindings;
mod wheel;
mod wheel_tags;

#[derive(Error, Debug)]
pub enum Error {
    #[error(transparent)]
    IO(#[from] io::Error),
    /// This shouldn't actually be possible to occur
    #[error("Failed to serialize direct_url.json ಠ_ಠ")]
    DirectUrlSerdeJson(#[source] serde_json::Error),
    /// Tags/metadata didn't match platform
    #[error("The wheel is incompatible with the current platform {os} {arch}")]
    IncompatibleWheel { os: Os, arch: Arch },
    /// The wheel is broken
    #[error("The wheel is invalid: {0}")]
    InvalidWheel(String),
    /// pyproject.toml or poetry.lock are broken
    #[error("The poetry dependency specification (pyproject.toml or poetry.lock) is broken (try `poetry update`?): {0}")]
    InvalidPoetry(String),
    /// Doesn't follow file name schema
    #[error("The wheel filename \"{0}\" is invalid: {1}")]
    InvalidWheelFileName(String, String),
    /// The wheel is broken, but in python pkginfo
    #[error("The wheel is broken")]
    PkgInfo(#[from] python_pkginfo::Error),
    #[error("Failed to read the wheel file {0}")]
    Zip(String, #[source] ZipError),
    #[error("Failed to run python subcommand")]
    PythonSubcommand(#[source] io::Error),
    #[error("Failed to move data files")]
    WalkDir(#[from] walkdir::Error),
    #[error("RECORD file doesn't match wheel contents: {0}")]
    RecordFile(String),
    #[error("RECORD file is invalid")]
    RecordCsv(#[from] csv::Error),
    #[error("Broken virtualenv: {0}")]
    BrokenVenv(String),
    #[error("Failed to detect the operating system version: {0}")]
    OsVersionDetection(String),
    #[error("Failed to detect the current platform")]
    PlatformInfo(#[source] PlatformInfoError),
    #[error("Invalid version specification, only none or == is supported")]
    Pep440,
}

/// High level API: Install a wheel in a virtualenv
///
/// Returns the tag of the wheel
pub fn install_wheel_in_venv(
    wheel: &Path,
    venv: &Path,
    interpreter: &Path,
    major: u8,
    minor: u8,
) -> Result<String, Error> {
    let venv_base = venv.canonicalize()?;
    let location = InstallLocation::Venv {
        venv_base,
        python_version: (major, minor),
    };
    let locked_dir = location.acquire_lock()?;

    install_wheel(
        &locked_dir,
        wheel,
        false,
        &[],
        // Only relevant for monotrail style installation
        "",
        interpreter,
    )
}
