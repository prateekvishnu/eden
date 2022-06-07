/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#[cfg(feature = "fb")]
use configparser::hg::generate_dynamicconfig;

use super::define_flags;
use super::Repo;
use super::Result;
use super::IO;

define_flags! {
    pub struct DebugDynamicConfigOpts {
        /// Host name to fetch a canary config from.
        canary: Option<String>,
    }
}

pub fn run(opts: DebugDynamicConfigOpts, _io: &IO, repo: &mut Repo) -> Result<u8> {
    #[cfg(feature = "fb")]
    {
        let username = repo
            .config()
            .get("ui", "username")
            .and_then(|u| Some(u.to_string()))
            .unwrap_or_else(|| "".to_string());

        generate_dynamicconfig(
            Some(repo.shared_dot_hg_path()),
            repo.repo_name(),
            opts.canary,
            username,
        )?;
    }
    #[cfg(not(feature = "fb"))]
    let _ = (opts, repo);

    Ok(0)
}

pub fn name() -> &'static str {
    "debugdynamicconfig"
}

pub fn doc() -> &'static str {
    "generate the dynamic configuration"
}

pub fn synopsis() -> Option<&'static str> {
    None
}
