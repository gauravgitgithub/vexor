//! Running an app as another user (PM2 `uid`/`gid`): names or numbers
//! resolved to ids. Only root can switch users, so a spec asking for one
//! is refused unless Vexor runs as root.

use std::ffi::CString;

/// The ids an app runs with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ids {
    pub uid: Option<u32>,
    pub gid: Option<u32>,
}

/// Resolve `user` (name or number) and `group` (name or number). Without
/// a group, the user's primary group is used.
pub fn resolve(user: Option<&str>, group: Option<&str>) -> Result<Ids, String> {
    if user.is_none() && group.is_none() {
        return Ok(Ids {
            uid: None,
            gid: None,
        });
    }
    if !rustix::process::geteuid().is_root() {
        return Err("`user`/`group` (PM2 uid/gid) need Vexor running as root".into());
    }
    let (uid, primary) = match user {
        None => (None, None),
        Some(u) => {
            let (uid, gid) = passwd(u)?;
            (Some(uid), gid)
        }
    };
    let gid = match group {
        Some(g) => Some(group_id(g)?),
        None => primary,
    };
    // Fail closed: switching the user must never leave the app in root's
    // group (gid 0), which it would keep from the shim.
    if let Some(u) = uid
        && u != 0
    {
        match gid {
            None => {
                return Err(format!(
                    "user {} has no passwd entry, so its group is unknown: set `group` (PM2 gid)",
                    user.unwrap_or("?")
                ));
            }
            Some(0) => {
                return Err(
                    "group 0 (root) for a non-root user is refused: set another `group`".into(),
                );
            }
            Some(_) => {}
        }
    }
    Ok(Ids { uid, gid })
}

/// (uid, primary gid) of a user name, or of a numeric uid (whose primary
/// group is unknown if it has no passwd entry).
#[allow(unsafe_code)]
fn passwd(user: &str) -> Result<(u32, Option<u32>), String> {
    let mut pw = std::mem::MaybeUninit::<libc::passwd>::zeroed();
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    let mut out: *mut libc::passwd = std::ptr::null_mut();
    let numeric = user.parse::<u32>().ok();
    // SAFETY: `pw`, `buf` and `out` are valid for the call; `buf` outlives
    // every use of the result (only its ids are read).
    let rc = unsafe {
        match numeric {
            Some(uid) => libc::getpwuid_r(
                uid,
                pw.as_mut_ptr(),
                buf.as_mut_ptr(),
                buf.len(),
                &raw mut out,
            ),
            None => {
                let name = CString::new(user).map_err(|_| format!("invalid user name {user:?}"))?;
                libc::getpwnam_r(
                    name.as_ptr(),
                    pw.as_mut_ptr(),
                    buf.as_mut_ptr(),
                    buf.len(),
                    &raw mut out,
                )
            }
        }
    };
    if rc == 0 && !out.is_null() {
        // SAFETY: the call succeeded and filled `pw`.
        let pw = unsafe { pw.assume_init() };
        return Ok((pw.pw_uid, Some(pw.pw_gid)));
    }
    match numeric {
        Some(uid) => Ok((uid, None)),
        None => Err(format!("no user named {user:?}")),
    }
}

#[allow(unsafe_code)]
fn group_id(group: &str) -> Result<u32, String> {
    if let Ok(gid) = group.parse::<u32>() {
        return Ok(gid);
    }
    let name = CString::new(group).map_err(|_| format!("invalid group name {group:?}"))?;
    let mut gr = std::mem::MaybeUninit::<libc::group>::zeroed();
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    let mut out: *mut libc::group = std::ptr::null_mut();
    // SAFETY: as in `passwd`.
    let rc = unsafe {
        libc::getgrnam_r(
            name.as_ptr(),
            gr.as_mut_ptr(),
            buf.as_mut_ptr(),
            buf.len(),
            &raw mut out,
        )
    };
    if rc == 0 && !out.is_null() {
        // SAFETY: the call succeeded and filled `gr`.
        let gr = unsafe { gr.assume_init() };
        return Ok(gr.gr_gid);
    }
    Err(format!("no group named {group:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_requested_needs_nothing() {
        assert_eq!(
            resolve(None, None),
            Ok(Ids {
                uid: None,
                gid: None
            })
        );
    }

    #[test]
    fn switching_users_needs_root() {
        if rustix::process::geteuid().is_root() {
            assert!(resolve(Some("root"), None).is_ok());
            // A uid without a passwd entry needs an explicit, non-root group.
            let err = resolve(Some("4000123"), None).expect_err("no group");
            assert!(err.contains("group"), "{err}");
            assert!(resolve(Some("4000123"), Some("0")).is_err());
            assert_eq!(
                resolve(Some("4000123"), Some("4000123")),
                Ok(Ids {
                    uid: Some(4_000_123),
                    gid: Some(4_000_123)
                })
            );
            assert!(resolve(Some("no-such-user-vexor"), None).is_err());
            assert!(resolve(None, Some("no-such-group-vexor")).is_err());
        } else {
            let err = resolve(Some("root"), None).expect_err("not root");
            assert!(err.contains("root"), "{err}");
        }
    }
}
