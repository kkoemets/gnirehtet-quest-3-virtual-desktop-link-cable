use std::{fs, io, path::Path};

use thiserror::Error;

include!(concat!(env!("OUT_DIR"), "/embedded_apk.rs"));

pub fn materialize(destination: &Path) -> Result<(), EmbeddedApkError> {
    let apk = EMBEDDED_APK.ok_or(EmbeddedApkError::Missing)?;
    if apk.len() < 4 || &apk[..2] != b"PK" {
        return Err(EmbeddedApkError::Invalid);
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::read(destination).is_ok_and(|existing| existing == apk) {
        return Ok(());
    }
    fs::write(destination, apk)?;
    Ok(())
}

#[derive(Debug, Error)]
pub enum EmbeddedApkError {
    #[error(
        "matching Android v4 APK is not embedded; set GNIREHTET_VD_APK at build time or build android-v4 first"
    )]
    Missing,
    #[error("embedded Android artifact is not an APK/ZIP")]
    Invalid,
    #[error("could not materialize embedded Android APK: {0}")]
    Io(#[from] io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn development_build_has_an_explicit_missing_gate() {
        if EMBEDDED_APK.is_none() {
            assert!(matches!(
                materialize(Path::new("unused.apk")),
                Err(EmbeddedApkError::Missing)
            ));
        }
    }
}
