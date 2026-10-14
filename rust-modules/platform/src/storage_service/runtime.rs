//! One independently published rendezvous per installed service identity.
use super::wire::ErrorCode;
use std::path::Path;

pub fn app_identity(executable: &Path) -> Result<&str, ErrorCode> {
    if executable.file_name().and_then(|n| n.to_str()) != Some("pitv-storage") {
        return Err(ErrorCode::Invalid);
    }
    let dir = executable.parent().ok_or(ErrorCode::Invalid)?;
    if dir
        .parent()
        .and_then(Path::file_name)
        .and_then(|n| n.to_str())
        != Some("services")
    {
        return Err(ErrorCode::Invalid);
    }
    let app_id = dir
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|name| name.strip_suffix(".storage"))
        .ok_or(ErrorCode::Invalid)?;
    super::state::Flavor::from_app_id(app_id).ok_or(ErrorCode::Invalid)?;
    Ok(app_id)
}


// Process-start snapshot: never read a later activation's nonce on a failure path.





#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nightly_app_identity() {
        assert_eq!(app_identity(Path::new("/media/developer/apps/usr/palm/services/com.beb.plxnative.nightly.storage/pitv-storage")), Ok("com.beb.plxnative.nightly"));
    }

    #[test]
    fn rejects_foreign_or_malformed_install_paths() {
        for path in [
            "/usr/palm/services/com.beb.plxnative.typo.storage/pitv-storage",
            "/usr/palm/services/com.beb.plxnative.nightly/pitv-storage",
            "/usr/palm/applications/com.beb.plxnative.nightly.storage/pitv-storage",
            "/usr/palm/services/com.beb.plxnative.nightly.storage/other",
            "pitv-storage",
        ] {
            assert_eq!(app_identity(Path::new(path)), Err(ErrorCode::Invalid));
        }
    }

    #[test]
    fn packaged_app_identity() {
        let Ok(paths) = std::env::var("PLX_TEST_PACKAGED_HELPERS") else {
            return;
        };
        for path in paths.lines() {
            assert!(
                app_identity(Path::new(path)).is_ok(),
                "rejected packaged helper: {path}"
            );
        }
    }
}
