/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::io::Write;
use std::time::Instant;

use revisionstore::scmstore::activitylogger;
use revisionstore::scmstore::FileStoreBuilder;

use super::define_flags;
use super::Repo;
use super::Result;
use super::IO;

define_flags! {
    pub struct DebugScmStoreReplayOpts {
        /// Path to JSON activity log
        path: String,
    }
}

pub fn run(opts: DebugScmStoreReplayOpts, io: &IO, repo: &mut Repo) -> Result<u8> {
    // TODO: Take into account log timings to yield a more faithful
    // reproduction of fetch activity, particularly concurrent fetches.

    let file_builder = FileStoreBuilder::new(repo.config());
    let store = file_builder.local_path(repo.store_path()).build()?;

    let mut stdout = io.output();
    let mut stderr = io.error();

    let (mut key_count, mut fetch_count) = (0, 0);
    let start_instant = Instant::now();
    for log in activitylogger::log_iter(opts.path)? {
        let log = log?;
        match log.op {
            activitylogger::ActivityType::FileFetch => {
                key_count += log.keys.len();
                fetch_count += 1;
                let result = store.fetch(log.keys.into_iter(), log.attrs);
                match result.missing() {
                    Ok(failed) => {
                        if failed.len() > 0 {
                            write!(stderr, "Failed to fetch keys {:?}\n", failed)?;
                        }
                    }
                    Err(err) => write!(stderr, "Fetch error: {:#?}\n", err)?,
                };
            }
        }
    }

    write!(
        stdout,
        "Fetched {} keys across {} fetches in {:?}\n",
        key_count,
        fetch_count,
        start_instant.elapsed()
    )?;

    Ok(0)
}

pub fn name() -> &'static str {
    "debugscmstorereplay"
}

pub fn doc() -> &'static str {
    "replay scmstore activity log"
}

pub fn synopsis() -> Option<&'static str> {
    None
}
