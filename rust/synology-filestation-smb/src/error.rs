//! Map `smb2` errors onto the workspace-wide [`SynoFsError`] so SMB failures
//! classify the same way HTTP ones do.
//!
//! The important property preserved here is the **transient-vs-definitive**
//! split, readable via [`SynoFsError::category`]:
//!
//! * connection loss / timeout / session-expiry / sharing-violation →
//!   [`SynoFsError::Io`] (category `Transport`) — a future transport-selection
//!   layer treats this as "SMB unhealthy, fall back to the HTTP path";
//! * not-found / permission → definitive; they must propagate unchanged so we
//!   never mask a real "file isn't there" behind a retry or a fallback.

use smb2::ErrorKind;
use synology_filestation_core::SynoFsError;

/// Map an [`smb2::Error`] to a [`SynoFsError`].
pub fn to_syno_error(err: &smb2::Error) -> SynoFsError {
    kind_to_syno(err.kind(), &err.to_string())
}

/// Whether the server turned the credentials down: the logon-rejection
/// family (`STATUS_LOGON_FAILURE`, a locked or expired account, …). Each one
/// is a strike towards DSM's auto-block.
///
/// Narrower than "maps to `PermissionDenied`" on purpose. Signing and
/// access-denied failures map there too, but they are the server's
/// configuration rather than the account's, cost no strike, and are fixed on
/// the NAS — remembering them as a refusal kept SMB off for a day after the
/// fix, with advice about domains that did not apply.
pub fn is_login_refusal(err: &smb2::Error) -> bool {
    err.kind() == ErrorKind::AuthRequired
}

/// [`is_login_refusal`], after a session-setup failure has been mapped by
/// [`session_error`]: a refusal is the one `LoginFailed` this crate makes.
pub fn is_refused_login(err: &SynoFsError) -> bool {
    // The exact shape, not any `LoginFailed`: core wraps HTTP re-login
    // failures in it too, and one handed back through a redial is not the
    // SMB server refusing the account.
    matches!(err, SynoFsError::LoginFailed(inner) if matches!(**inner, SynoFsError::PermissionDenied))
}

/// Map a failure to build a session. As [`to_syno_error`], except that a
/// refused login becomes `LoginFailed(PermissionDenied)`, so the difference
/// between a refusal and every other `PermissionDenied` survives the trip.
/// The category is unchanged.
pub(crate) fn session_error(err: &smb2::Error) -> SynoFsError {
    if is_login_refusal(err) {
        tracing::debug!(error = %err, "SMB: the server refused the login");
        SynoFsError::LoginFailed(Box::new(SynoFsError::PermissionDenied))
    } else {
        to_syno_error(err)
    }
}

/// Map `smb2`'s high-level [`ErrorKind`] to a [`SynoFsError`]. `context` is the
/// human-readable detail carried into the `Io` variants.
pub(crate) fn kind_to_syno(kind: ErrorKind, context: &str) -> SynoFsError {
    match kind {
        ErrorKind::NotFound => SynoFsError::NotFound,
        // Bad/rejected credentials, no access, or a signing requirement all mean
        // "this principal can't read it" to a caller.
        ErrorKind::AccessDenied | ErrorKind::AuthRequired | ErrorKind::SigningRequired => {
            SynoFsError::PermissionDenied
        }
        ErrorKind::AlreadyExists => SynoFsError::AlreadyExists,
        ErrorKind::DiskFull => SynoFsError::NoSpace,
        ErrorKind::Unsupported => SynoFsError::NotSupported,
        // Caller/programmer errors: wrong entry type, or a single-read method
        // used on an oversized file (we always use the chunked reads, so this
        // shouldn't occur — surface as InvalidArg rather than hide it).
        ErrorKind::IsADirectory | ErrorKind::NotADirectory | ErrorKind::TooLarge => {
            SynoFsError::InvalidArg
        }
        // Transient / transport — category `Transport`, i.e. "fall back".
        ErrorKind::ConnectionLost
        | ErrorKind::TimedOut
        | ErrorKind::Io
        | ErrorKind::SessionExpired
        | ErrorKind::SharingViolation
        | ErrorKind::DfsReferral
        | ErrorKind::InvalidData
        | ErrorKind::Cancelled => SynoFsError::Io(format!("smb: {context}")),
        // `ErrorKind` is #[non_exhaustive]: unknown/future kinds are treated as
        // transport failures (safe default — the caller can fall back).
        _ => SynoFsError::Io(format!("smb: {context}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use synology_filestation_core::error::ErrorCategory;

    /// Regression (Copilot, #315): any `LoginFailed` counted, and core wraps
    /// HTTP re-login failures in it too — a TLS error or a DSM 400 handed
    /// back through a redial was latched as an SMB credential refusal.
    #[test]
    fn only_the_marker_session_error_makes_is_a_refused_login() {
        let marker = SynoFsError::LoginFailed(Box::new(SynoFsError::PermissionDenied));
        assert!(is_refused_login(&marker));
        assert!(is_refused_login(&session_error(&smb2::Error::Auth {
            message: "STATUS_LOGON_FAILURE".into()
        })));
        for other in [
            SynoFsError::LoginFailed(Box::new(SynoFsError::Io("tls handshake".into()))),
            SynoFsError::LoginFailed(Box::new(SynoFsError::ApiError(400))),
            SynoFsError::PermissionDenied,
        ] {
            assert!(!is_refused_login(&other), "{other:?}");
        }
    }

    fn cat(kind: ErrorKind) -> ErrorCategory {
        kind_to_syno(kind, "detail").category()
    }

    #[test]
    fn definitive_kinds_map_to_their_categories() {
        assert_eq!(cat(ErrorKind::NotFound), ErrorCategory::NotFound);
        assert_eq!(
            cat(ErrorKind::AccessDenied),
            ErrorCategory::PermissionDenied
        );
        assert_eq!(
            cat(ErrorKind::AuthRequired),
            ErrorCategory::PermissionDenied
        );
        assert_eq!(
            cat(ErrorKind::SigningRequired),
            ErrorCategory::PermissionDenied
        );
        assert_eq!(cat(ErrorKind::AlreadyExists), ErrorCategory::AlreadyExists);
        assert_eq!(cat(ErrorKind::DiskFull), ErrorCategory::NoSpace);
        assert_eq!(cat(ErrorKind::Unsupported), ErrorCategory::NotSupported);
        assert_eq!(cat(ErrorKind::IsADirectory), ErrorCategory::InvalidArg);
        assert_eq!(cat(ErrorKind::NotADirectory), ErrorCategory::InvalidArg);
        assert_eq!(cat(ErrorKind::TooLarge), ErrorCategory::InvalidArg);
    }

    #[test]
    fn transport_kinds_map_to_transport_so_selection_can_fall_back() {
        for kind in [
            ErrorKind::ConnectionLost,
            ErrorKind::TimedOut,
            ErrorKind::Io,
            ErrorKind::SessionExpired,
            ErrorKind::SharingViolation,
            ErrorKind::DfsReferral,
            ErrorKind::InvalidData,
            ErrorKind::Cancelled,
        ] {
            assert_eq!(cat(kind), ErrorCategory::Transport, "{kind:?}");
        }
    }

    #[test]
    fn to_syno_error_wires_kind_through_for_constructible_errors() {
        // A missing-file lookup stays definitive (NotFound category), never
        // Transport — so it won't be silently retried/fallback-masked.
        assert_eq!(
            to_syno_error(&smb2::Error::Timeout).category(),
            ErrorCategory::Transport
        );
        assert_eq!(
            to_syno_error(&smb2::Error::Disconnected).category(),
            ErrorCategory::Transport
        );
        assert_eq!(
            to_syno_error(&smb2::Error::Auth {
                message: "bad password".into(),
            })
            .category(),
            ErrorCategory::PermissionDenied
        );
        // Using a single-read method on an oversized file is a programmer error.
        assert_eq!(
            to_syno_error(&smb2::Error::FileTooLargeForSingleRead {
                size: 15_277_437,
                max_read: 8_388_608,
            })
            .category(),
            ErrorCategory::InvalidArg
        );
    }
}
