/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::anyhow;
use async_runtime::try_block_unless_interrupted as block_on;
use configmodel::convert::ByteCount;
use configmodel::Config;
use configmodel::ConfigExt;
use manifest_tree::Diff;
use manifest_tree::TreeManifest;
use pathmatcher::Matcher;
use storemodel::ReadFileContents;
use treestate::dirstate::Dirstate;
use treestate::metadata::Metadata;
use treestate::serialization::Serializable;
use treestate::treestate::TreeState;
use types::hgid::NULL_ID;
use types::HgId;
use util::file::atomic_open;
use util::file::atomic_write;
use util::path::remove_file;
use vfs::VFS;
use workingcopy::sparse;

use crate::file_state;
use crate::ActionMap;
use crate::Checkout;

static CONFIG_OVERRIDE_CACHE: &str = "sparseprofileconfigs";

/// A somewhat simplified/specialized checkout suitable for use during a clone.
pub fn checkout(
    config: &dyn Config,
    wc_path: &Path,
    source_mf: &TreeManifest,
    target_mf: &TreeManifest,
    file_store: Arc<dyn ReadFileContents<Error = anyhow::Error> + Send + Sync>,
    ts: &mut TreeState,
    target: HgId,
) -> anyhow::Result<()> {
    let dot_hg = wc_path.join(".hg");

    let _wlock = repolock::lock_working_copy(config, &dot_hg)?;

    let mut sparse_overrides = None;

    let matcher: Box<dyn Matcher> = match fs::read_to_string(dot_hg.join("sparse")) {
        Ok(contents) => {
            let overrides = sparse::config_overrides(config);
            sparse_overrides = Some(overrides.clone());
            Box::new(sparse::sparse_matcher(
                sparse::Root::from_bytes(contents.as_bytes(), ".hg/sparse".to_string())?,
                target_mf.clone(),
                file_store.clone(),
                overrides,
            )?)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            Box::new(pathmatcher::AlwaysMatcher::new())
        }
        Err(e) => {
            return Err(e.into());
        }
    };

    let diff = Diff::new(source_mf, target_mf, &matcher)?;
    let actions = ActionMap::from_diff(diff)?;

    let vfs = VFS::new(wc_path.to_path_buf())?;
    let checkout = Checkout::from_config(vfs.clone(), config)?;
    let mut plan = checkout.plan_action_map(actions);

    if config.get_or_default("checkout", "resumable")? {
        let progress_path = dot_hg.join("updateprogress");
        plan.add_progress(progress_path)?;
    }

    atomic_write(&dot_hg.join("updatestate"), |f| {
        f.write_all(target.to_hex().as_bytes())
    })?;

    block_on(plan.apply_store(&file_store))?;

    let ts_meta = Metadata(BTreeMap::from([("p1".to_string(), target.to_hex())]));
    let mut ts_buf: Vec<u8> = Vec::new();
    ts_meta.serialize(&mut ts_buf)?;
    ts.set_metadata(&ts_buf);

    // Probably not required for clone.
    for removed in plan.removed_files() {
        ts.remove(removed)?;
    }

    for updated in plan
        .updated_content_files()
        .chain(plan.updated_meta_files())
    {
        let fstate = file_state(&vfs, updated)?;
        ts.insert(updated, &fstate)?;
    }

    // TODO: invalidate treestate mtime

    flush_dirstate(config, ts, &dot_hg, target)?;

    remove_file(dot_hg.join("updatestate"))?;

    if let Some(sparse_overrides) = sparse_overrides {
        atomic_write(&dot_hg.join(CONFIG_OVERRIDE_CACHE), |f| {
            serde_json::to_writer(f, &sparse_overrides)?;
            Ok(())
        })?;
    }

    Ok(())
}

fn flush_dirstate(
    config: &dyn Config,
    ts: &mut TreeState,
    dot_hg_path: &Path,
    target: HgId,
) -> anyhow::Result<()> {
    // Flush treestate then write .hg/dirstate that points to the
    // current treestate file.

    let dirstate_path = dot_hg_path.join("dirstate");
    let mut dirstate_file = atomic_open(&dirstate_path)?;

    // Get "now" from the atomic temp file we just created's mtime.
    // This ensures we use a sane mtime in case the file system
    // doesn't match our local clock, for whatever reason.
    let now = dirstate_file
        .as_file()
        .metadata()?
        .modified()?
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_secs();

    // Invalidate entries with mtime >= now so we can notice user
    // edits to files in the same second the checkout completes.
    ts.invalidate_mtime(now.try_into()?)?;

    let tree_root_id = ts.flush()?;

    let tree_file = ts
        .path()
        .file_name()
        .ok_or_else(|| anyhow!("bad treestate path: {:?}", ts.path()))?;

    let mut threshold = 0;
    let min_repack_threshold = config
        .get_or_default::<ByteCount>("treestate", "minrepackthreshold")?
        .value();
    if tree_root_id.0 > min_repack_threshold {
        if let Some(factor) = config.get_nonempty_opt::<u64>("treestate", "repackfactor")? {
            threshold = tree_root_id.0 * factor;
        }
    }
    let ds = Dirstate {
        p0: target,
        p1: NULL_ID,
        tree_filename: tree_file.to_owned().into_string().map_err(|_| {
            anyhow!(
                "can't convert treestate file name to String: {:?}",
                tree_file
            )
        })?,
        tree_root_id,
        repack_threshold: Some(threshold),
    };

    ds.serialize(dirstate_file.as_file())?;

    dirstate_file.save()?;

    Ok(())
}
