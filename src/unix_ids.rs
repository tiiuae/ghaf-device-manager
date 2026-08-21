// SPDX-FileCopyrightText: 2026 TII (SSRC) and the Ghaf contributors
// SPDX-License-Identifier: Apache-2.0

//! Name-to-id lookups for the Unix users and groups named in the configuration.
//!
//! `nix` wraps the reentrant `getpwnam_r`/`getgrnam_r` calls, so these lookups
//! are safe to run from the async runtime's worker threads.

use anyhow::{Context, Result, bail};
use nix::unistd::{Group, User};

pub(crate) fn user_id(name: &str) -> Result<u32> {
    match User::from_name(name).with_context(|| format!("failed to look up user {name}"))? {
        Some(user) => Ok(user.uid.as_raw()),
        None => bail!("unknown user {name}"),
    }
}

pub(crate) fn group_id(name: &str) -> Result<u32> {
    match Group::from_name(name).with_context(|| format!("failed to look up group {name}"))? {
        Some(group) => Ok(group.gid.as_raw()),
        None => bail!("unknown group {name}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_the_root_account() {
        assert_eq!(user_id("root").unwrap(), 0);
    }

    #[test]
    fn rejects_unknown_names() {
        assert!(user_id("ghaf-no-such-user").is_err());
        assert!(group_id("ghaf-no-such-group").is_err());
    }

    #[test]
    fn rejects_names_containing_nul() {
        assert!(user_id("root\0extra").is_err());
        assert!(group_id("root\0extra").is_err());
    }
}
