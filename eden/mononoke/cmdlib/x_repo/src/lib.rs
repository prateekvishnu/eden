/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Scaffolding that's generally useful to build CLI tools on top of Mononoke.

use anyhow::{bail, Error};
use blobrepo::BlobRepo;
use cacheblob::LeaseOps;
use cmdlib::args::{self, MononokeMatches};
use context::CoreContext;
use cross_repo_sync::{
    create_commit_syncer_lease, create_commit_syncers,
    types::{Source, Target},
    CommitSyncRepos, CommitSyncer, Syncers,
};
use futures_util::try_join;
use live_commit_sync_config::{CfgrLiveCommitSyncConfig, LiveCommitSyncConfig};
use sql_construct::SqlConstructFromMetadataDatabaseConfig;
use std::sync::Arc;
use synced_commit_mapping::SqlSyncedCommitMapping;

/// Instantiate the `Syncers` struct by parsing `matches`
pub async fn create_commit_syncers_from_matches(
    ctx: &CoreContext,
    matches: &MononokeMatches<'_>,
) -> Result<Syncers<SqlSyncedCommitMapping>, Error> {
    let (source_repo, target_repo, mapping, live_commit_sync_config) =
        get_things_from_matches(ctx, matches).await?;

    let common_config = live_commit_sync_config.get_common_config(source_repo.0.get_repoid())?;

    let caching = matches.caching();
    let x_repo_syncer_lease = create_commit_syncer_lease(ctx.fb, caching)?;

    let large_repo_id = common_config.large_repo_id;
    let source_repo_id = source_repo.0.get_repoid();
    let target_repo_id = target_repo.0.get_repoid();
    let (small_repo, large_repo) = if large_repo_id == source_repo_id {
        (target_repo.0, source_repo.0)
    } else if large_repo_id == target_repo_id {
        (source_repo.0, target_repo.0)
    } else {
        bail!(
            "Unexpectedly CommitSyncConfig {:?} has neither of {}, {} as a large repo",
            common_config,
            source_repo_id,
            target_repo_id
        );
    };

    create_commit_syncers(
        ctx,
        small_repo,
        large_repo,
        mapping,
        live_commit_sync_config,
        x_repo_syncer_lease,
    )
}

/// Instantiate the source-target `CommitSyncer` struct by parsing `matches`
pub async fn create_commit_syncer_from_matches(
    ctx: &CoreContext,
    matches: &MononokeMatches<'_>,
) -> Result<CommitSyncer<SqlSyncedCommitMapping>, Error> {
    create_commit_syncer_from_matches_impl(ctx, matches, false /* reverse */).await
}

/// Instantiate the target-source `CommitSyncer` struct by parsing `matches`
pub async fn create_reverse_commit_syncer_from_matches(
    ctx: &CoreContext,
    matches: &MononokeMatches<'_>,
) -> Result<CommitSyncer<SqlSyncedCommitMapping>, Error> {
    create_commit_syncer_from_matches_impl(ctx, matches, true /* reverse */).await
}

/// Instantiate some auxiliary things from `matches`
/// Naming is hard.
async fn get_things_from_matches(
    ctx: &CoreContext,
    matches: &MononokeMatches<'_>,
) -> Result<
    (
        Source<BlobRepo>,
        Target<BlobRepo>,
        SqlSyncedCommitMapping,
        Arc<dyn LiveCommitSyncConfig>,
    ),
    Error,
> {
    let fb = ctx.fb;
    let logger = ctx.logger();

    let config_store = matches.config_store();
    let source_repo_id = args::get_source_repo_id(config_store, &matches)?;
    let target_repo_id = args::get_target_repo_id(config_store, &matches)?;

    let (_, source_repo_config) =
        args::get_config_by_repoid(config_store, &matches, source_repo_id)?;
    let (_, target_repo_config) =
        args::get_config_by_repoid(config_store, &matches, target_repo_id)?;

    if source_repo_config.storage_config.metadata != target_repo_config.storage_config.metadata {
        return Err(Error::msg(
            "source repo and target repo have different metadata database configs!",
        ));
    }

    let mysql_options = matches.mysql_options();
    let readonly_storage = matches.readonly_storage();

    let mapping = SqlSyncedCommitMapping::with_metadata_database_config(
        ctx.fb,
        &source_repo_config.storage_config.metadata,
        &mysql_options,
        readonly_storage.0,
    )?;

    let source_repo_fut = args::open_repo_with_repo_id(fb, logger, source_repo_id, &matches);
    let target_repo_fut = args::open_repo_with_repo_id(fb, logger, target_repo_id, &matches);

    let (source_repo, target_repo) = try_join!(source_repo_fut, target_repo_fut)?;

    let live_commit_sync_config: Arc<dyn LiveCommitSyncConfig> =
        Arc::new(CfgrLiveCommitSyncConfig::new(&ctx.logger(), config_store)?);

    Ok((
        Source(source_repo),
        Target(target_repo),
        mapping,
        live_commit_sync_config,
    ))
}

fn flip_direction<T>(source_item: Source<T>, target_item: Target<T>) -> (Source<T>, Target<T>) {
    (Source(target_item.0), Target(source_item.0))
}

async fn create_commit_syncer_from_matches_impl(
    ctx: &CoreContext,
    matches: &MononokeMatches<'_>,
    reverse: bool,
) -> Result<CommitSyncer<SqlSyncedCommitMapping>, Error> {
    let (source_repo, target_repo, mapping, live_commit_sync_config) =
        get_things_from_matches(ctx, matches).await?;

    let (source_repo, target_repo) = if reverse {
        flip_direction(source_repo, target_repo)
    } else {
        (source_repo, target_repo)
    };

    let caching = matches.caching();
    let x_repo_syncer_lease = create_commit_syncer_lease(ctx.fb, caching)?;

    create_commit_syncer(
        ctx,
        source_repo,
        target_repo,
        mapping,
        live_commit_sync_config,
        x_repo_syncer_lease,
    )
    .await
}

async fn create_commit_syncer<'a>(
    ctx: &'a CoreContext,
    source_repo: Source<BlobRepo>,
    target_repo: Target<BlobRepo>,
    mapping: SqlSyncedCommitMapping,
    live_commit_sync_config: Arc<dyn LiveCommitSyncConfig>,
    x_repo_syncer_lease: Arc<dyn LeaseOps>,
) -> Result<CommitSyncer<SqlSyncedCommitMapping>, Error> {
    let common_config = live_commit_sync_config.get_common_config(source_repo.0.get_repoid())?;

    let repos = CommitSyncRepos::new(source_repo.0, target_repo.0, &common_config)?;
    let commit_syncer = CommitSyncer::new(
        ctx,
        mapping,
        repos,
        live_commit_sync_config,
        x_repo_syncer_lease,
    );
    Ok(commit_syncer)
}
