/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use crate::{
    BundleResolverError, BundleResolverResultExt, InfiniteBookmarkPush, NonFastForwardPolicy,
    PlainBookmarkPush, PostResolveAction, PostResolveBookmarkOnlyPushRebase,
    PostResolveInfinitePush, PostResolvePush, PostResolvePushRebase, PushrebaseBookmarkSpec,
};
use anyhow::{anyhow, Context, Error, Result};
use blobrepo::scribe::{log_commits_to_scribe_raw, ScribeCommitInfo};
use bookmarks::{BookmarkName, BookmarkUpdateReason, BundleReplay};
use bookmarks_movement::{BookmarkMovementError, BookmarkUpdatePolicy, BookmarkUpdateTargets};
use bytes::Bytes;
use context::CoreContext;
use hooks::HookManager;
use mercurial_bundle_replay_data::BundleReplayData;
use mercurial_mutation::HgMutationStoreRef;
use metaconfig_types::{BookmarkAttrs, InfinitepushParams, PushParams, PushrebaseParams};
use mononoke_types::{BonsaiChangeset, ChangesetId};
use pushrebase::PushrebaseError;
#[cfg(fbcode_build)]
use pushrebase_client::SCSPushrebaseClient;
use pushrebase_client::{LocalPushrebaseClient, PushrebaseClient};

use reachabilityindex::LeastCommonAncestorsHint;
use repo_identity::RepoIdentityRef;
use repo_read_write_status::RepoReadWriteFetcher;
use scribe_commit_queue::ChangedFilesInfo;
use slog::debug;
use stats::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use trait_alias::trait_alias;
use tunables::tunables;

use crate::hook_running::{map_hook_rejections, HookRejectionRemapper};
use crate::rate_limits::enforce_commit_rate_limits;
use crate::response::{
    UnbundleBookmarkOnlyPushRebaseResponse, UnbundleInfinitePushResponse,
    UnbundlePushRebaseResponse, UnbundlePushResponse, UnbundleResponse,
};
use crate::CrossRepoPushSource;

define_stats! {
    prefix = "mononoke.unbundle.processed";
    push: dynamic_timeseries("{}.push", (reponame: String); Rate, Sum),
    pushrebase: dynamic_timeseries("{}.pushrebase", (reponame: String); Rate, Sum),
    bookmark_only_pushrebase: dynamic_timeseries("{}.bookmark_only_pushrebase", (reponame: String); Rate, Sum),
    infinitepush: dynamic_timeseries("{}.infinitepush", (reponame: String); Rate, Sum),
}

#[trait_alias]
pub trait Repo = bookmarks_movement::Repo + HgMutationStoreRef;

pub async fn run_post_resolve_action(
    ctx: &CoreContext,
    repo: &impl Repo,
    bookmark_attrs: &BookmarkAttrs,
    lca_hint: &Arc<dyn LeastCommonAncestorsHint>,
    infinitepush_params: &InfinitepushParams,
    pushrebase_params: &PushrebaseParams,
    push_params: &PushParams,
    hook_manager: &HookManager,
    readonly_fetcher: &RepoReadWriteFetcher,
    action: PostResolveAction,
    cross_repo_push_source: CrossRepoPushSource,
) -> Result<UnbundleResponse, BundleResolverError> {
    enforce_commit_rate_limits(ctx, &action).await?;

    // FIXME: it's used not only in pushrebase, so it worth moving
    // populate_git_mapping outside of PushrebaseParams.
    let unbundle_response = match action {
        PostResolveAction::Push(action) => run_push(
            ctx,
            repo,
            bookmark_attrs,
            lca_hint,
            hook_manager,
            infinitepush_params,
            pushrebase_params,
            readonly_fetcher,
            action,
            push_params,
            cross_repo_push_source,
        )
        .await
        .context("While doing a push")
        .map(UnbundleResponse::Push)?,
        PostResolveAction::InfinitePush(action) => run_infinitepush(
            ctx,
            repo,
            bookmark_attrs,
            lca_hint,
            hook_manager,
            infinitepush_params,
            pushrebase_params,
            readonly_fetcher,
            action,
            cross_repo_push_source,
        )
        .await
        .context("While doing an infinitepush")
        .map(UnbundleResponse::InfinitePush)?,
        PostResolveAction::PushRebase(action) => run_pushrebase(
            ctx,
            repo,
            bookmark_attrs,
            lca_hint,
            infinitepush_params,
            pushrebase_params,
            hook_manager,
            readonly_fetcher,
            action,
            cross_repo_push_source,
        )
        .await
        .map(UnbundleResponse::PushRebase)?,
        PostResolveAction::BookmarkOnlyPushRebase(action) => run_bookmark_only_pushrebase(
            ctx,
            repo,
            bookmark_attrs,
            lca_hint,
            hook_manager,
            infinitepush_params,
            pushrebase_params,
            readonly_fetcher,
            action,
            cross_repo_push_source,
        )
        .await
        .context("While doing a bookmark-only pushrebase")
        .map(UnbundleResponse::BookmarkOnlyPushRebase)?,
    };
    report_unbundle_type(repo, &unbundle_response);
    Ok(unbundle_response)
}

fn report_unbundle_type(repo: &impl RepoIdentityRef, unbundle_response: &UnbundleResponse) {
    let repo_name = repo.repo_identity().name().to_string();
    match unbundle_response {
        UnbundleResponse::Push(_) => STATS::push.add_value(1, (repo_name,)),
        UnbundleResponse::PushRebase(_) => STATS::pushrebase.add_value(1, (repo_name,)),
        UnbundleResponse::InfinitePush(_) => STATS::infinitepush.add_value(1, (repo_name,)),
        UnbundleResponse::BookmarkOnlyPushRebase(_) => {
            STATS::bookmark_only_pushrebase.add_value(1, (repo_name,))
        }
    }
}

async fn run_push(
    ctx: &CoreContext,
    repo: &impl Repo,
    bookmark_attrs: &BookmarkAttrs,
    lca_hint: &Arc<dyn LeastCommonAncestorsHint>,
    hook_manager: &HookManager,
    infinitepush_params: &InfinitepushParams,
    pushrebase_params: &PushrebaseParams,
    readonly_fetcher: &RepoReadWriteFetcher,
    action: PostResolvePush,
    push_params: &PushParams,
    cross_repo_push_source: CrossRepoPushSource,
) -> Result<UnbundlePushResponse, BundleResolverError> {
    debug!(ctx.logger(), "unbundle processing: running push.");
    let PostResolvePush {
        changegroup_id,
        mut bookmark_pushes,
        mutations,
        maybe_raw_bundle2_id,
        maybe_pushvars,
        non_fast_forward_policy,
        uploaded_bonsais,
        uploaded_hg_changeset_ids,
        hook_rejection_remapper,
    } = action;

    if tunables().get_mutation_accept_for_infinitepush() {
        repo.hg_mutation_store()
            .add_entries(ctx, uploaded_hg_changeset_ids, mutations)
            .await
            .context("Failed to store mutation data")?;
    }

    if bookmark_pushes.len() > 1 {
        return Err(anyhow!(
            "only push to at most one bookmark is allowed, got {:?}",
            bookmark_pushes
        )
        .into());
    }

    let mut changesets_to_log = vec![];
    let mut new_changesets = HashMap::new();
    for bcs in uploaded_bonsais {
        let changeset_id = bcs.get_changeset_id();
        changesets_to_log.push(ScribeCommitInfo {
            changeset_id,
            bubble_id: None,
            changed_files: ChangedFilesInfo::new(&bcs),
        });
        new_changesets.insert(changeset_id, bcs);
    }

    let mut bookmark_ids = Vec::new();
    let mut maybe_bookmark = None;
    if let Some(bookmark_push) = bookmark_pushes.pop() {
        bookmark_ids.push(bookmark_push.part_id);
        let bundle_replay_data = maybe_raw_bundle2_id.map(BundleReplayData::new);
        let bundle_replay_data = bundle_replay_data
            .as_ref()
            .map(|data| data as &dyn BundleReplay);

        plain_push_bookmark(
            ctx,
            repo,
            lca_hint,
            infinitepush_params,
            pushrebase_params,
            bookmark_attrs,
            hook_manager,
            &bookmark_push,
            new_changesets,
            non_fast_forward_policy,
            BookmarkUpdateReason::Push,
            maybe_pushvars.as_ref(),
            bundle_replay_data,
            hook_rejection_remapper.as_ref(),
            cross_repo_push_source,
            readonly_fetcher,
        )
        .await?;

        maybe_bookmark = Some(bookmark_push.name);
    }

    log_commits_to_scribe_raw(
        ctx,
        repo,
        maybe_bookmark.as_ref(),
        changesets_to_log,
        push_params.commit_scribe_category.as_deref(),
    )
    .await;

    Ok(UnbundlePushResponse {
        changegroup_id,
        bookmark_ids,
    })
}

async fn run_infinitepush(
    ctx: &CoreContext,
    repo: &impl Repo,
    bookmark_attrs: &BookmarkAttrs,
    lca_hint: &Arc<dyn LeastCommonAncestorsHint>,
    hook_manager: &HookManager,
    infinitepush_params: &InfinitepushParams,
    pushrebase_params: &PushrebaseParams,
    readonly_fetcher: &RepoReadWriteFetcher,
    action: PostResolveInfinitePush,
    cross_repo_push_source: CrossRepoPushSource,
) -> Result<UnbundleInfinitePushResponse, BundleResolverError> {
    debug!(ctx.logger(), "unbundle processing: running infinitepush");
    let PostResolveInfinitePush {
        changegroup_id,
        maybe_bookmark_push,
        mutations,
        maybe_raw_bundle2_id,
        uploaded_bonsais,
        uploaded_hg_changeset_ids,
    } = action;

    if tunables().get_mutation_accept_for_infinitepush() {
        repo.hg_mutation_store()
            .add_entries(ctx, uploaded_hg_changeset_ids, mutations)
            .await
            .context("Failed to store mutation data")?;
    }

    let bookmark = match maybe_bookmark_push {
        Some(bookmark_push) => {
            let bundle_replay_data = maybe_raw_bundle2_id.map(BundleReplayData::new);
            let bundle_replay_data = bundle_replay_data
                .as_ref()
                .map(|data| data as &dyn BundleReplay);

            infinitepush_scratch_bookmark(
                ctx,
                repo,
                lca_hint,
                infinitepush_params,
                pushrebase_params,
                bookmark_attrs,
                hook_manager,
                &bookmark_push,
                bundle_replay_data,
                cross_repo_push_source,
                readonly_fetcher,
            )
            .await?;

            Some(bookmark_push.name)
        }
        None => None,
    };

    log_commits_to_scribe_raw(
        ctx,
        repo,
        bookmark.as_ref(),
        uploaded_bonsais
            .iter()
            .map(|bcs| ScribeCommitInfo {
                changeset_id: bcs.get_changeset_id(),
                bubble_id: None,
                changed_files: ChangedFilesInfo::new(&bcs),
            })
            .collect(),
        infinitepush_params.commit_scribe_category.as_deref(),
    )
    .await;

    Ok(UnbundleInfinitePushResponse { changegroup_id })
}

async fn run_pushrebase(
    ctx: &CoreContext,
    repo: &impl Repo,
    bookmark_attrs: &BookmarkAttrs,
    lca_hint: &Arc<dyn LeastCommonAncestorsHint>,
    infinitepush_params: &InfinitepushParams,
    pushrebase_params: &PushrebaseParams,
    hook_manager: &HookManager,
    readonly_fetcher: &RepoReadWriteFetcher,
    action: PostResolvePushRebase,
    cross_repo_push_source: CrossRepoPushSource,
) -> Result<UnbundlePushRebaseResponse, BundleResolverError> {
    debug!(ctx.logger(), "unbundle processing: running pushrebase.");
    let PostResolvePushRebase {
        bookmark_push_part_id,
        bookmark_spec,
        maybe_hg_replay_data,
        maybe_pushvars,
        commonheads,
        uploaded_bonsais,
        hook_rejection_remapper,
    } = action;
    let changed_files_info: Vec<_> = uploaded_bonsais.iter().map(ChangedFilesInfo::new).collect();

    // FIXME: stop cloning when this fn is async
    let bookmark = bookmark_spec.get_bookmark_name().clone();

    let (pushrebased_rev, pushrebased_changesets) = match bookmark_spec {
        // There's no `.context()` after `normal_pushrebase`, as it has
        // `Error=BundleResolverError` and doing `.context("bla").from_err()`
        // would turn some useful variant of `BundleResolverError` into generic
        // `BundleResolverError::Error`, which in turn would render incorrectly
        // (see definition of `BundleResolverError`).
        PushrebaseBookmarkSpec::NormalPushrebase(onto_bookmark) => {
            let (pushrebased_rev, pushrebased_changesets) = normal_pushrebase(
                ctx,
                repo,
                &pushrebase_params,
                lca_hint,
                uploaded_bonsais,
                &onto_bookmark,
                maybe_pushvars.as_ref(),
                &maybe_hg_replay_data,
                bookmark_attrs,
                infinitepush_params,
                hook_manager,
                hook_rejection_remapper.as_ref(),
                cross_repo_push_source,
                readonly_fetcher,
            )
            .await?;
            let new_commits: Vec<ChangesetId> =
                pushrebased_changesets.iter().map(|p| p.id_new).collect();
            log_commits_to_scribe_raw(
                ctx,
                repo,
                Some(&bookmark),
                new_commits
                    .into_iter()
                    .zip(changed_files_info.into_iter())
                    .map(|(changeset_id, changed_files)| ScribeCommitInfo {
                        changeset_id,
                        bubble_id: None,
                        changed_files,
                    })
                    .collect(),
                pushrebase_params.commit_scribe_category.as_deref(),
            )
            .await;
            (pushrebased_rev, pushrebased_changesets)
        }
        PushrebaseBookmarkSpec::ForcePushrebase(plain_push) => {
            let changesets_to_log = uploaded_bonsais
                .iter()
                .map(|bcs| ScribeCommitInfo {
                    changeset_id: bcs.get_changeset_id(),
                    bubble_id: None,
                    changed_files: ChangedFilesInfo::new(&bcs),
                })
                .collect();

            let (pushrebased_rev, pushrebased_changesets) = force_pushrebase(
                ctx,
                repo,
                &pushrebase_params,
                lca_hint,
                hook_manager,
                uploaded_bonsais,
                plain_push,
                maybe_pushvars.as_ref(),
                &maybe_hg_replay_data,
                bookmark_attrs,
                infinitepush_params,
                hook_rejection_remapper.as_ref(),
                cross_repo_push_source,
                readonly_fetcher,
            )
            .await
            .context("While doing a force pushrebase")?;
            log_commits_to_scribe_raw(
                ctx,
                repo,
                Some(&bookmark),
                changesets_to_log,
                pushrebase_params.commit_scribe_category.as_deref(),
            )
            .await;
            (pushrebased_rev, pushrebased_changesets)
        }
    };

    repo.phases()
        .add_reachable_as_public(ctx, vec![pushrebased_rev.clone()])
        .await
        .context("While marking pushrebased changeset as public")?;

    Ok(UnbundlePushRebaseResponse {
        commonheads,
        pushrebased_rev,
        pushrebased_changesets,
        onto: bookmark,
        bookmark_push_part_id,
    })
}

async fn run_bookmark_only_pushrebase(
    ctx: &CoreContext,
    repo: &impl Repo,
    bookmark_attrs: &BookmarkAttrs,
    lca_hint: &Arc<dyn LeastCommonAncestorsHint>,
    hook_manager: &HookManager,
    infinitepush_params: &InfinitepushParams,
    pushrebase_params: &PushrebaseParams,
    readonly_fetcher: &RepoReadWriteFetcher,
    action: PostResolveBookmarkOnlyPushRebase,
    cross_repo_push_source: CrossRepoPushSource,
) -> Result<UnbundleBookmarkOnlyPushRebaseResponse, BundleResolverError> {
    debug!(
        ctx.logger(),
        "unbundle processing: running bookmark-only pushrebase."
    );
    let PostResolveBookmarkOnlyPushRebase {
        bookmark_push,
        maybe_raw_bundle2_id,
        maybe_pushvars,
        non_fast_forward_policy,
        hook_rejection_remapper,
    } = action;

    let part_id = bookmark_push.part_id;
    let bundle_replay_data = maybe_raw_bundle2_id.map(BundleReplayData::new);
    let bundle_replay_data = bundle_replay_data
        .as_ref()
        .map(|data| data as &dyn BundleReplay);

    // This is a bookmark-only push, so there are no new changesets.
    let new_changesets = HashMap::new();

    plain_push_bookmark(
        ctx,
        repo,
        lca_hint,
        infinitepush_params,
        pushrebase_params,
        bookmark_attrs,
        hook_manager,
        &bookmark_push,
        new_changesets,
        non_fast_forward_policy,
        BookmarkUpdateReason::Pushrebase,
        maybe_pushvars.as_ref(),
        bundle_replay_data,
        hook_rejection_remapper.as_ref(),
        cross_repo_push_source,
        readonly_fetcher,
    )
    .await?;

    Ok(UnbundleBookmarkOnlyPushRebaseResponse {
        bookmark_push_part_id: part_id,
    })
}

fn should_use_scs() -> bool {
    let pct = tunables()
        .get_pushrebase_redirect_to_scs_pct()
        .clamp(0, 100) as f64
        / 100.0;
    cfg!(fbcode_build) && rand::random::<f64>() < pct
}

async fn normal_pushrebase<'a>(
    ctx: &'a CoreContext,
    repo: &'a impl Repo,
    pushrebase_params: &'a PushrebaseParams,
    lca_hint: &Arc<dyn LeastCommonAncestorsHint>,
    changesets: HashSet<BonsaiChangeset>,
    bookmark: &'a BookmarkName,
    maybe_pushvars: Option<&'a HashMap<String, Bytes>>,
    maybe_hg_replay_data: &'a Option<pushrebase::HgReplayData>,
    bookmark_attrs: &'a BookmarkAttrs,
    infinitepush_params: &'a InfinitepushParams,
    hook_manager: &'a HookManager,
    hook_rejection_remapper: &'a dyn HookRejectionRemapper,
    cross_repo_push_source: CrossRepoPushSource,
    readonly_fetcher: &RepoReadWriteFetcher,
) -> Result<(ChangesetId, Vec<pushrebase::PushrebaseChangesetPair>), BundleResolverError> {
    let repo_name = repo.repo_identity().name().to_string();
    let result = if should_use_scs() {
        #[cfg(fbcode_build)]
        {
            if let Ok(host_port) = std::env::var("SCS_SERVER_HOST_PORT") {
                SCSPushrebaseClient::from_host_port(ctx.fb, host_port)?
            } else {
                SCSPushrebaseClient::new(ctx.fb)?
            }
            .pushrebase(repo_name, bookmark, changesets, maybe_pushvars)
            .await
        }
        #[cfg(not(fbcode_build))]
        unreachable!()
    } else {
        LocalPushrebaseClient {
            ctx,
            repo,
            pushrebase_params,
            lca_hint,
            maybe_hg_replay_data,
            bookmark_attrs,
            infinitepush_params,
            hook_manager,
            cross_repo_push_source,
            readonly_fetcher,
        }
        .pushrebase(repo_name, bookmark, changesets, maybe_pushvars)
        .await
    };
    match result {
        Ok(outcome) => Ok((outcome.head, outcome.rebased_changesets)),
        Err(err) => match err {
            BookmarkMovementError::PushrebaseError(PushrebaseError::Conflicts(conflicts)) => {
                Err(BundleResolverError::PushrebaseConflicts(conflicts))
            }
            BookmarkMovementError::HookFailure(rejections) => {
                let rejections = map_hook_rejections(rejections, hook_rejection_remapper).await?;
                Err(BundleResolverError::HookError(rejections))
            }
            _ => Err(BundleResolverError::Error(err.into())),
        },
    }
}

async fn force_pushrebase(
    ctx: &CoreContext,
    repo: &impl Repo,
    pushrebase_params: &PushrebaseParams,
    lca_hint: &Arc<dyn LeastCommonAncestorsHint>,
    hook_manager: &HookManager,
    uploaded_bonsais: HashSet<BonsaiChangeset>,
    bookmark_push: PlainBookmarkPush<ChangesetId>,
    maybe_pushvars: Option<&HashMap<String, Bytes>>,
    maybe_hg_replay_data: &Option<pushrebase::HgReplayData>,
    bookmark_attrs: &BookmarkAttrs,
    infinitepush_params: &InfinitepushParams,
    hook_rejection_remapper: &dyn HookRejectionRemapper,
    cross_repo_push_source: CrossRepoPushSource,
    readonly_fetcher: &RepoReadWriteFetcher,
) -> Result<(ChangesetId, Vec<pushrebase::PushrebaseChangesetPair>), BundleResolverError> {
    let new_target = bookmark_push
        .new
        .ok_or_else(|| anyhow!("new changeset is required for force pushrebase"))?;

    let mut new_changesets = HashMap::new();
    for bcs in uploaded_bonsais {
        let cs_id = bcs.get_changeset_id();
        new_changesets.insert(cs_id, bcs);
    }

    let bundle_replay_data = if let Some(hg_replay_data) = &maybe_hg_replay_data {
        Some(hg_replay_data.to_bundle_replay_data(None).await?)
    } else {
        None
    };
    let bundle_replay_data = bundle_replay_data
        .as_ref()
        .map(|data| data as &dyn BundleReplay);

    plain_push_bookmark(
        ctx,
        repo,
        lca_hint,
        infinitepush_params,
        pushrebase_params,
        bookmark_attrs,
        hook_manager,
        &bookmark_push,
        new_changesets,
        NonFastForwardPolicy::Allowed,
        BookmarkUpdateReason::Pushrebase,
        maybe_pushvars,
        bundle_replay_data,
        hook_rejection_remapper,
        cross_repo_push_source,
        readonly_fetcher,
    )
    .await?;

    // Note that this push did not do any actual rebases, so we do not
    // need to provide any actual mapping, an empty Vec will do
    Ok((new_target, Vec::new()))
}

async fn plain_push_bookmark(
    ctx: &CoreContext,
    repo: &impl Repo,
    lca_hint: &Arc<dyn LeastCommonAncestorsHint>,
    infinitepush_params: &InfinitepushParams,
    pushrebase_params: &PushrebaseParams,
    bookmark_attrs: &BookmarkAttrs,
    hook_manager: &HookManager,
    bookmark_push: &PlainBookmarkPush<ChangesetId>,
    new_changesets: HashMap<ChangesetId, BonsaiChangeset>,
    non_fast_forward_policy: NonFastForwardPolicy,
    reason: BookmarkUpdateReason,
    maybe_pushvars: Option<&HashMap<String, Bytes>>,
    bundle_replay_data: Option<&dyn BundleReplay>,
    hook_rejection_remapper: &dyn HookRejectionRemapper,
    cross_repo_push_source: CrossRepoPushSource,
    readonly_fetcher: &RepoReadWriteFetcher,
) -> Result<(), BundleResolverError> {
    match (bookmark_push.old, bookmark_push.new) {
        (None, Some(new_target)) => {
            let res =
                bookmarks_movement::CreateBookmarkOp::new(&bookmark_push.name, new_target, reason)
                    .only_if_public()
                    .with_new_changesets(new_changesets)
                    .with_pushvars(maybe_pushvars)
                    .with_bundle_replay_data(bundle_replay_data)
                    .with_push_source(cross_repo_push_source)
                    .run(
                        ctx,
                        repo,
                        lca_hint,
                        infinitepush_params,
                        pushrebase_params,
                        bookmark_attrs,
                        hook_manager,
                        readonly_fetcher,
                    )
                    .await;
            match res {
                Ok(()) => {}
                Err(err) => match err {
                    BookmarkMovementError::HookFailure(rejections) => {
                        let rejections =
                            map_hook_rejections(rejections, hook_rejection_remapper).await?;
                        return Err(BundleResolverError::HookError(rejections));
                    }
                    _ => {
                        return Err(BundleResolverError::Error(
                            Error::from(err).context("Failed to create bookmark"),
                        ));
                    }
                },
            }
        }

        (Some(old_target), Some(new_target)) => {
            let res = bookmarks_movement::UpdateBookmarkOp::new(
                &bookmark_push.name,
                BookmarkUpdateTargets {
                    old: old_target,
                    new: new_target,
                },
                if non_fast_forward_policy == NonFastForwardPolicy::Allowed {
                    BookmarkUpdatePolicy::AnyPermittedByConfig
                } else {
                    BookmarkUpdatePolicy::FastForwardOnly
                },
                reason,
            )
            .only_if_public()
            .with_new_changesets(new_changesets)
            .with_pushvars(maybe_pushvars)
            .with_bundle_replay_data(bundle_replay_data)
            .with_push_source(cross_repo_push_source)
            .run(
                ctx,
                repo,
                lca_hint,
                infinitepush_params,
                pushrebase_params,
                bookmark_attrs,
                hook_manager,
                readonly_fetcher,
            )
            .await;
            match res {
                Ok(()) => {}
                Err(err) => match err {
                    BookmarkMovementError::HookFailure(rejections) => {
                        let rejections =
                            map_hook_rejections(rejections, hook_rejection_remapper).await?;
                        return Err(BundleResolverError::HookError(rejections));
                    }
                    _ => {
                        return Err(BundleResolverError::Error(Error::from(err).context(
                            if non_fast_forward_policy == NonFastForwardPolicy::Allowed {
                                "Failed to move bookmark"
                            } else {
                                concat!(
                                    "Failed to fast-forward bookmark (set pushvar ",
                                    "NON_FAST_FORWARD=true for a non-fast-forward move)",
                                )
                            },
                        )));
                    }
                },
            }
        }

        (Some(old_target), None) => {
            bookmarks_movement::DeleteBookmarkOp::new(&bookmark_push.name, old_target, reason)
                .only_if_public()
                .with_pushvars(maybe_pushvars)
                .with_bundle_replay_data(bundle_replay_data)
                .run(
                    ctx,
                    repo,
                    infinitepush_params,
                    bookmark_attrs,
                    readonly_fetcher,
                )
                .await
                .context("Failed to delete bookmark")?;
        }

        (None, None) => {}
    }
    Ok(())
}

async fn infinitepush_scratch_bookmark(
    ctx: &CoreContext,
    repo: &impl Repo,
    lca_hint: &Arc<dyn LeastCommonAncestorsHint>,
    infinitepush_params: &InfinitepushParams,
    pushrebase_params: &PushrebaseParams,
    bookmark_attrs: &BookmarkAttrs,
    hook_manager: &HookManager,
    bookmark_push: &InfiniteBookmarkPush<ChangesetId>,
    bundle_replay_data: Option<&dyn BundleReplay>,
    cross_repo_push_source: CrossRepoPushSource,
    readonly_fetcher: &RepoReadWriteFetcher,
) -> Result<()> {
    if bookmark_push.old.is_none() && bookmark_push.create {
        bookmarks_movement::CreateBookmarkOp::new(
            &bookmark_push.name,
            bookmark_push.new,
            BookmarkUpdateReason::Push,
        )
        .only_if_scratch()
        .with_bundle_replay_data(bundle_replay_data)
        .with_push_source(cross_repo_push_source)
        .run(
            ctx,
            repo,
            lca_hint,
            infinitepush_params,
            pushrebase_params,
            bookmark_attrs,
            hook_manager,
            readonly_fetcher,
        )
        .await
        .context("Failed to create scratch bookmark")?;
    } else {
        let old_target = bookmark_push.old.ok_or_else(|| {
            anyhow!(
                "Unknown bookmark: {}. Use --create to create one.",
                bookmark_push.name
            )
        })?;
        bookmarks_movement::UpdateBookmarkOp::new(
            &bookmark_push.name,
            BookmarkUpdateTargets {
                old: old_target,
                new: bookmark_push.new,
            },
            if bookmark_push.force {
                BookmarkUpdatePolicy::AnyPermittedByConfig
            } else {
                BookmarkUpdatePolicy::FastForwardOnly
            },
            BookmarkUpdateReason::Push,
        )
        .only_if_scratch()
        .with_bundle_replay_data(bundle_replay_data)
        .with_push_source(cross_repo_push_source)
        .run(
            ctx,
            repo,
            lca_hint,
            infinitepush_params,
            pushrebase_params,
            bookmark_attrs,
            hook_manager,
            readonly_fetcher,
        )
        .await
        .context(if bookmark_push.force {
            "Failed to move scratch bookmark"
        } else {
            "Failed to fast-forward scratch bookmark (try --force?)"
        })?;
    }

    Ok(())
}
