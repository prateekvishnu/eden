/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![feature(auto_traits)]
#![feature(async_closure)]
#![deny(warnings)]

/// Mononoke -> hg sync job
///
/// It's a special job that is used to synchronize Mononoke to Mercurial when Mononoke is a source
/// of truth. All writes to Mononoke are replayed to Mercurial using this job. That can be used
/// to verify Mononoke's correctness and/or use hg as a disaster recovery mechanism.
use anyhow::{bail, format_err, Error, Result};
use blobrepo::BlobRepo;
use bookmarks::{BookmarkName, BookmarkUpdateLog, BookmarkUpdateLogEntry, Freshness};
use borrowed::borrowed;
use bundle_generator::FilenodeVerifier;
use bundle_preparer::{maybe_adjust_batch, BundlePreparer};
use clap_old::{Arg, ArgGroup, SubCommand};
use cloned::cloned;
use cmdlib::{
    args::{self, MononokeMatches},
    helpers::block_execute,
};
use context::CoreContext;
use darkstorm_verifier::DarkstormVerifier;
use dbbookmarks::SqlBookmarksBuilder;
use fbinit::FacebookInit;
use futures::{
    future::{self, try_join, try_join3, BoxFuture, FutureExt as _, TryFutureExt},
    pin_mut,
    stream::{self, StreamExt, TryStreamExt},
    Stream,
};
use futures_stats::{futures03::TimedFutureExt, FutureStats};
use futures_watchdog::WatchdogExt;
use http::Uri;
use lfs_verifier::LfsVerifier;
use mercurial_types::HgChangesetId;
use metaconfig_types::HgsqlName;
use metaconfig_types::RepoReadOnly;
use mononoke_api_types::InnerRepo;
use mononoke_types::ChangesetId;
use mutable_counters::{ArcMutableCounters, MutableCountersArc};
use regex::Regex;
use repo_read_write_status::{RepoReadWriteFetcher, SqlRepoReadWriteStatus};
use retry::{retry, RetryAttemptsCount};
use scuba_ext::MononokeScubaSampleBuilder;
use slog::{error, info};
use sql_construct::{facebook::FbSqlConstruct, SqlConstruct};
use sql_ext::facebook::MysqlOptions;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tempfile::NamedTempFile;

mod bundle_generator;
mod bundle_preparer;
mod darkstorm_verifier;
mod errors;
mod globalrev_syncer;
mod hgrepo;
mod lfs_verifier;

use errors::{
    ErrorKind::SyncFailed,
    PipelineError::{self, AnonymousError, EntryError},
};
use globalrev_syncer::GlobalrevSyncer;
use hgrepo::{list_hg_server_bookmarks, HgRepo};
use hgserver_config::ServerConfig;

const ARG_BOOKMARK_REGEX_FORCE_GENERATE_LFS: &str = "bookmark-regex-force-generate-lfs";
const ARG_BOOKMARK_MOVE_ANY_DIRECTION: &str = "bookmark-move-any-direction";
const ARG_USE_HG_SERVER_BOOKMARK_VALUE_IF_MISMATCH: &str =
    "use-hg-server-bookmark-value-if-mismatch";
const ARG_DARKSTORM_BACKUP_REPO_GROUP: &str = "darkstorm-backup-repo";
const ARG_DARKSTORM_BACKUP_REPO_ID: &str = "darkstorm-backup-repo-id";
const ARG_DARKSTORM_BACKUP_REPO_NAME: &str = "darkstorm-backup-repo-name";
const ARG_BYPASS_READONLY: &str = "bypass-readonly";
const GENERATE_BUNDLES: &str = "generate-bundles";
const MODE_SYNC_ONCE: &str = "sync-once";
const MODE_SYNC_LOOP: &str = "sync-loop";
const LATEST_REPLAYED_REQUEST_KEY: &str = "latest-replayed-request";
const SLEEP_SECS: u64 = 1;
const SCUBA_TABLE: &str = "mononoke_hg_sync";
const UNLOCK_REASON: &str = "Unlocked by successful sync";
const LOCK_REASON: &str = "Locked due to sync failure, check Source Control @ FB";

const HGSQL_GLOBALREVS_USE_SQLITE: &str = "hgsql-globalrevs-use-sqlite";
const HGSQL_GLOBALREVS_DB_ADDR: &str = "hgsql-globalrevs-db-addr";

const DEFAULT_RETRY_NUM: usize = 3;
const DEFAULT_BATCH_SIZE: usize = 10;
const DEFAULT_SINGLE_BUNDLE_TIMEOUT_MS: u64 = 5 * 60 * 1000;

const CONFIGERATOR_HGSERVER_PATH: &str = "scm/mononoke/hgserverconf/hgserver";

#[derive(Copy, Clone)]
struct QueueSize(usize);

struct PipelineState<T> {
    entries: Vec<BookmarkUpdateLogEntry>,
    data: T,
}

type OutcomeWithStats =
    Result<(FutureStats, PipelineState<RetryAttemptsCount>), (Option<FutureStats>, PipelineError)>;

type Outcome = Result<PipelineState<RetryAttemptsCount>, PipelineError>;

fn get_id_to_search_after(entries: &[BookmarkUpdateLogEntry]) -> i64 {
    entries.iter().map(|entry| entry.id).max().unwrap_or(0)
}

fn bind_sync_err(entries: &[BookmarkUpdateLogEntry], cause: Error) -> PipelineError {
    let ids: Vec<i64> = entries.iter().map(|entry| entry.id).collect();
    let entries = entries.to_vec();
    EntryError {
        entries,
        cause: (SyncFailed { ids, cause }).into(),
    }
}

fn bind_sync_result<T>(
    entries: &[BookmarkUpdateLogEntry],
    res: Result<T>,
) -> Result<PipelineState<T>, PipelineError> {
    match res {
        Ok(data) => Ok(PipelineState {
            entries: entries.to_vec(),
            data,
        }),
        Err(cause) => Err(bind_sync_err(entries, cause)),
    }
}

fn drop_outcome_stats(o: OutcomeWithStats) -> Outcome {
    o.map(|(_, r)| r).map_err(|(_, e)| e)
}

fn build_reporting_handler<'a, B>(
    ctx: &'a CoreContext,
    scuba_sample: &'a MononokeScubaSampleBuilder,
    retry_num: usize,
    bookmarks: &'a B,
) -> impl Fn(OutcomeWithStats) -> BoxFuture<'a, Result<PipelineState<RetryAttemptsCount>, PipelineError>>
where
    B: BookmarkUpdateLog,
{
    move |res| {
        async move {
            let log_entries = match &res {
                Ok((_, pipeline_state, ..)) => Some(pipeline_state.entries.clone()),
                Err((_, EntryError { entries, .. })) => Some(entries.clone()),
                Err((_, AnonymousError { .. })) => None,
            };

            let maybe_stats = match &res {
                Ok((stats, _)) => Some(stats),
                Err((stats, _)) => stats.as_ref(),
            };

            // TODO: (torozco) T43766262 We should embed attempts in retry()'s Error type and use it
            // here instead of receiving a plain ErrorKind and implicitly assuming retry_num attempts.
            let attempts = match &res {
                Ok((_, PipelineState { data: attempts, .. })) => attempts.clone(),
                Err(..) => RetryAttemptsCount(retry_num),
            };

            let maybe_error = match &res {
                Ok(..) => None,
                Err((_, EntryError { cause, .. })) => Some(cause),
                Err((_, AnonymousError { cause, .. })) => Some(cause),
            };

            let f = async {
                if let Some(log_entries) = log_entries {
                    let duration =
                        maybe_stats.map_or_else(|| Duration::from_secs(0), |s| s.completion_time);

                    let error = maybe_error.map(|e| format!("{:?}", e));
                    let next_id = get_id_to_search_after(&log_entries);

                    let n = bookmarks
                        .count_further_bookmark_log_entries(ctx.clone(), next_id as u64, None)
                        .await?;
                    let queue_size = QueueSize(n as usize);
                    info!(
                        ctx.logger(),
                        "queue size after processing: {}", queue_size.0
                    );
                    log_processed_entries_to_scuba(
                        &log_entries,
                        scuba_sample.clone(),
                        error,
                        attempts,
                        duration,
                        queue_size,
                    );
                }
                Result::<_, Error>::Ok(())
            };

            // Ignore result from future that did the logging
            let _ = f.await;
            drop_outcome_stats(res)
        }
        .boxed()
    }
}

fn get_read_write_fetcher(
    fb: FacebookInit,
    mysql_options: &MysqlOptions,
    repo_lock_db_addr: Option<&str>,
    hgsql_name: HgsqlName,
    lock_on_failure: bool,
    use_sqlite: bool,
    readonly_storage: bool,
) -> Result<(Option<RepoReadWriteFetcher>, RepoReadWriteFetcher)> {
    let unlock_via: Result<RepoReadWriteFetcher> = match repo_lock_db_addr {
        Some(repo_lock_db_addr) => {
            let sql_repo_read_write_status = if use_sqlite {
                let path = Path::new(repo_lock_db_addr);
                SqlRepoReadWriteStatus::with_sqlite_path(path, readonly_storage)
            } else {
                SqlRepoReadWriteStatus::with_mysql(
                    fb,
                    repo_lock_db_addr.to_string(),
                    mysql_options,
                    readonly_storage,
                )
            };
            sql_repo_read_write_status.and_then(|connection| {
                Ok(RepoReadWriteFetcher::new(
                    Some(connection),
                    RepoReadOnly::ReadWrite,
                    hgsql_name,
                ))
            })
        }
        None => {
            if lock_on_failure {
                Err(Error::msg(
                    "repo_lock_db_addr not specified with lock_on_failure",
                ))
            } else {
                Ok(RepoReadWriteFetcher::new(
                    None,
                    RepoReadOnly::ReadWrite,
                    hgsql_name,
                ))
            }
        }
    };

    unlock_via.and_then(|v| {
        let lock_via = if lock_on_failure {
            Some(v.clone())
        } else {
            None
        };
        Ok((lock_via, v))
    })
}

async fn unlock_repo_if_locked(
    ctx: &CoreContext,
    read_write_fetcher: &RepoReadWriteFetcher,
) -> Result<(), Error> {
    let repo_state = read_write_fetcher.readonly().await?;

    match repo_state {
        RepoReadOnly::ReadOnly(ref lock_msg) if lock_msg == LOCK_REASON => {
            let updated = read_write_fetcher
                .set_mononoke_read_write(&UNLOCK_REASON.to_string())
                .await?;
            if updated {
                info!(ctx.logger(), "repo is unlocked");
            }
            Ok(())
        }
        RepoReadOnly::ReadOnly(..) | RepoReadOnly::ReadWrite => Ok(()),
    }
}

async fn lock_repo_if_unlocked(
    ctx: &CoreContext,
    read_write_fetcher: &RepoReadWriteFetcher,
) -> Result<(), Error> {
    info!(ctx.logger(), "locking repo...");
    let repo_state = read_write_fetcher.readonly().await?;

    match repo_state {
        RepoReadOnly::ReadWrite => {
            let updated = read_write_fetcher
                .set_read_only(&LOCK_REASON.to_string())
                .await?;
            if updated {
                info!(ctx.logger(), "repo is locked now");
            }
            Ok(())
        }

        RepoReadOnly::ReadOnly(ref lock_msg) => {
            info!(ctx.logger(), "repo is locked already: {}", lock_msg);
            Ok(())
        }
    }
}

fn build_outcome_handler<'a>(
    ctx: &'a CoreContext,
    lock_via: &'a Option<RepoReadWriteFetcher>,
) -> impl Fn(Outcome) -> BoxFuture<'a, Result<Vec<BookmarkUpdateLogEntry>, Error>> {
    move |res| {
        async move {
            match res {
                Ok(PipelineState { entries, .. }) => {
                    info!(
                        ctx.logger(),
                        "successful sync of entries {:?}",
                        entries.iter().map(|c| c.id).collect::<Vec<_>>()
                    );
                    Ok(entries)
                }
                Err(AnonymousError { cause: e }) => {
                    info!(ctx.logger(), "error without entry");
                    Err(e)
                }
                Err(EntryError { cause: e, .. }) => match &lock_via {
                    Some(repo_read_write_fetcher) => {
                        let _ = lock_repo_if_unlocked(ctx, repo_read_write_fetcher).await;
                        Err(e)
                    }
                    None => Err(e),
                },
            }
        }
        .boxed()
    }
}

#[derive(Clone)]
pub struct CombinedBookmarkUpdateLogEntry {
    components: Vec<BookmarkUpdateLogEntry>,
    bundle_file: Arc<NamedTempFile>,
    timestamps_file: Arc<NamedTempFile>,
    cs_id: Option<(ChangesetId, HgChangesetId)>,
    bookmark: BookmarkName,
    // List of commits in a bundle in case they are known
    commits: CommitsInBundle,
}

#[derive(Clone)]
pub enum CommitsInBundle {
    Commits(Vec<(HgChangesetId, ChangesetId)>),
    Unknown,
}

/// Sends a downloaded bundle to hg
async fn try_sync_single_combined_entry(
    ctx: &CoreContext,
    attempt: usize,
    combined_entry: &CombinedBookmarkUpdateLogEntry,
    hg_repo: &HgRepo,
) -> Result<(), Error> {
    let ids: Vec<_> = combined_entry
        .components
        .iter()
        .map(|entry| entry.id)
        .collect();
    info!(ctx.logger(), "syncing log entries {:?} ...", ids);

    let bundle_path = get_path(&combined_entry.bundle_file)?;
    let timestamps_path = get_path(&combined_entry.timestamps_file)?;

    hg_repo
        .apply_bundle(
            bundle_path,
            timestamps_path,
            combined_entry.bookmark.clone(),
            combined_entry.cs_id.map(|(_bcs_id, hg_cs_id)| hg_cs_id),
            attempt,
            ctx.logger(),
            &combined_entry.commits,
        )
        .watched(ctx.logger())
        .await?;

    Ok(())
}

async fn sync_single_combined_entry(
    ctx: &CoreContext,
    combined_entry: &CombinedBookmarkUpdateLogEntry,
    hg_repo: &HgRepo,
    base_retry_delay_ms: u64,
    retry_num: usize,
    globalrev_syncer: &GlobalrevSyncer,
) -> Result<RetryAttemptsCount, Error> {
    if combined_entry.cs_id.is_some() {
        globalrev_syncer
            .sync(ctx, &combined_entry.commits)
            .watched(ctx.logger())
            .await?
    }

    let (_, attempts) = retry(
        &ctx.logger(),
        |attempt| try_sync_single_combined_entry(&ctx, attempt, &combined_entry, &hg_repo),
        base_retry_delay_ms,
        retry_num,
    )
    .watched(ctx.logger())
    .await?;

    Ok(attempts)
}

/// Logs to Scuba information about a single bundle sync event
fn log_processed_entry_to_scuba(
    log_entry: &BookmarkUpdateLogEntry,
    mut scuba_sample: MononokeScubaSampleBuilder,
    error: Option<String>,
    attempts: RetryAttemptsCount,
    duration: Duration,
    queue_size: QueueSize,
    combined_from: Option<i64>,
) {
    let entry = log_entry.id;
    let book = format!("{}", log_entry.bookmark_name);
    let reason = format!("{}", log_entry.reason);
    let delay = log_entry.timestamp.since_seconds();

    scuba_sample
        .add("entry", entry)
        .add("bookmark", book)
        .add("reason", reason)
        .add("attempts", attempts.0)
        .add("duration", duration.as_millis() as i64);

    if let Some(combined_from) = combined_from {
        scuba_sample.add("combined_from", combined_from);
    }

    match error {
        Some(error) => {
            scuba_sample.add("success", 0).add("err", error);
        }
        None => {
            scuba_sample.add("success", 1).add("delay", delay);
            scuba_sample.add("queue_size", queue_size.0);
        }
    };

    scuba_sample.log();
}

fn log_processed_entries_to_scuba(
    entries: &[BookmarkUpdateLogEntry],
    scuba_sample: MononokeScubaSampleBuilder,
    error: Option<String>,
    attempts: RetryAttemptsCount,
    duration: Duration,
    queue_size: QueueSize,
) {
    let n: f64 = entries.len() as f64;
    let individual_duration = duration.div_f64(n);

    let combined_from = if entries.len() == 1 {
        // Set combined_from to None if we synced a single entry
        // This will make it easier to find entries that were batched
        None
    } else {
        entries.get(0).map(|entry| entry.id)
    };
    entries.iter().for_each(|entry| {
        log_processed_entry_to_scuba(
            entry,
            scuba_sample.clone(),
            error.clone(),
            attempts,
            individual_duration,
            queue_size,
            combined_from,
        )
    });
}

fn get_path(f: &NamedTempFile) -> Result<String> {
    f.path()
        .to_str()
        .map(|s| s.to_string())
        .ok_or(Error::msg("non-utf8 file"))
}

fn loop_over_log_entries<'a, B>(
    ctx: &'a CoreContext,
    bookmarks: &'a B,
    start_id: i64,
    loop_forever: bool,
    scuba_sample: &'a MononokeScubaSampleBuilder,
    fetch_up_to_bundles: u64,
    repo_read_write_fetcher: &'a RepoReadWriteFetcher,
) -> impl Stream<Item = Result<Vec<BookmarkUpdateLogEntry>, Error>> + 'a
where
    B: BookmarkUpdateLog + Clone,
{
    stream::try_unfold(Some(start_id), {
        let ctx = ctx.clone();
        let bookmarks = bookmarks.clone();
        move |maybe_id| {
            let ctx = ctx.clone();
            let bookmarks = bookmarks.clone();
            async move {
                match maybe_id {
                    Some(current_id) => {
                        let entries = bookmarks
                            .read_next_bookmark_log_entries_same_bookmark_and_reason(
                                ctx.clone(),
                                current_id as u64,
                                fetch_up_to_bundles,
                            )
                            .try_collect::<Vec<_>>()
                            .watched(ctx.logger())
                            .await?;

                        match entries.iter().last().cloned() {
                            None => {
                                if loop_forever {
                                    info!(ctx.logger(), "id: {}, no new entries found", current_id);
                                    scuba_sample.clone().add("success", 1).add("delay", 0).log();

                                    // First None means that no new entries will be added to the stream,
                                    // Some(current_id) means that bookmarks will be fetched again
                                    tokio::time::sleep(Duration::new(SLEEP_SECS, 0)).await;

                                    unlock_repo_if_locked(&ctx, &repo_read_write_fetcher)
                                        .watched(ctx.logger())
                                        .await?;
                                    Ok(Some((vec![], Some(current_id))))
                                } else {
                                    Ok(Some((vec![], None)))
                                }
                            }
                            Some(last_entry) => Ok(Some((entries, Some(last_entry.id)))),
                        }
                    }
                    None => Ok(None),
                }
            }
        }
    })
}

#[derive(Clone)]
pub struct BookmarkOverlay {
    bookmarks: Arc<HashMap<BookmarkName, ChangesetId>>,
    overlay: HashMap<BookmarkName, Option<ChangesetId>>,
}

impl BookmarkOverlay {
    fn new(bookmarks: Arc<HashMap<BookmarkName, ChangesetId>>) -> Self {
        Self {
            bookmarks,
            overlay: HashMap::new(),
        }
    }

    fn update(&mut self, book: BookmarkName, val: Option<ChangesetId>) {
        self.overlay.insert(book, val);
    }

    fn get_bookmark_values(&self) -> Vec<ChangesetId> {
        let mut res = vec![];
        for key in self.bookmarks.keys().chain(self.overlay.keys()) {
            if let Some(val) = self.overlay.get(key) {
                res.extend(val.clone().into_iter());
            } else if let Some(val) = self.bookmarks.get(key) {
                res.push(*val);
            }
        }

        res
    }

    fn is_in_overlay(&self, bookmark: &BookmarkName) -> bool {
        self.overlay.contains_key(bookmark)
    }

    fn get_value(&self, bookmark: &BookmarkName) -> Option<ChangesetId> {
        if let Some(value) = self.overlay.get(bookmark) {
            return value.clone();
        }
        self.bookmarks.get(bookmark).cloned()
    }
}

struct LatestReplayedSyncCounter {
    mutable_counters: ArcMutableCounters,
}

impl LatestReplayedSyncCounter {
    fn new(
        source_repo: &BlobRepo,
        darkstorm_backup_repo: Option<&BlobRepo>,
    ) -> Result<Self, Error> {
        if let Some(backup_repo) = darkstorm_backup_repo {
            let mutable_counters = backup_repo.mutable_counters_arc();
            Ok(Self { mutable_counters })
        } else {
            let mutable_counters = source_repo.mutable_counters_arc();
            Ok(Self { mutable_counters })
        }
    }

    async fn get_counter(&self, ctx: &CoreContext) -> Result<Option<i64>, Error> {
        self.mutable_counters
            .get_counter(ctx, LATEST_REPLAYED_REQUEST_KEY)
            .await
    }

    async fn set_counter(&self, ctx: &CoreContext, value: i64) -> Result<bool, Error> {
        self.mutable_counters
            .set_counter(
                ctx,
                LATEST_REPLAYED_REQUEST_KEY,
                value,
                // TODO(stash): do we need conditional updates here?
                None,
            )
            .await
    }
}

async fn run<'a>(ctx: CoreContext, matches: &'a MononokeMatches<'a>) -> Result<(), Error> {
    let hg_repo_path = match matches.value_of("hg-repo-ssh-path") {
        Some(hg_repo_path) => hg_repo_path.to_string(),
        None => {
            error!(ctx.logger(), "Path to hg repository must be specified");
            std::process::exit(1);
        }
    };

    let log_to_scuba = matches.is_present("log-to-scuba");
    let mut scuba_sample = if log_to_scuba {
        MononokeScubaSampleBuilder::new(ctx.fb, SCUBA_TABLE)
    } else {
        MononokeScubaSampleBuilder::with_discard()
    };
    scuba_sample.add_common_server_data();

    let mysql_options = matches.mysql_options();
    let readonly_storage = matches.readonly_storage();
    let config_store = matches.config_store();

    let repo_id = args::get_repo_id(config_store, matches).expect("need repo id");
    let (repo_name, repo_config) = args::get_config(config_store, matches)?;

    let base_retry_delay_ms = args::get_u64_opt(matches, "base-retry-delay-ms").unwrap_or(1000);
    let retry_num = args::get_usize(matches, "retry-num", DEFAULT_RETRY_NUM);

    let generate_bundles = matches.is_present(GENERATE_BUNDLES);
    let bookmark_regex_force_lfs = matches
        .value_of(ARG_BOOKMARK_REGEX_FORCE_GENERATE_LFS)
        .map(Regex::new)
        .transpose()?;

    let mut vars = HashMap::new();
    if matches.is_present(ARG_BOOKMARK_MOVE_ANY_DIRECTION) {
        vars.insert("NON_FAST_FORWARD".to_string(), bytes::Bytes::from("true"));
    }
    if matches.is_present(ARG_BYPASS_READONLY) {
        vars.insert("BYPASS_READONLY".to_string(), bytes::Bytes::from("true"));
    }

    let push_vars = if vars.is_empty() { None } else { Some(vars) };

    let lfs_params = repo_config.lfs.clone();

    let verify_lfs_blob_presence = matches
        .value_of("verify-lfs-blob-presence")
        .map(|s| s.to_string());

    let use_hg_server_bookmark_value_if_mismatch =
        matches.is_present(ARG_USE_HG_SERVER_BOOKMARK_VALUE_IF_MISMATCH);
    let maybe_darkstorm_backup_repo = if matches.is_present(ARG_DARKSTORM_BACKUP_REPO_ID)
        || matches.is_present(ARG_DARKSTORM_BACKUP_REPO_NAME)
    {
        let backup_repo_id = args::get_repo_id_from_value(
            config_store,
            &matches,
            ARG_DARKSTORM_BACKUP_REPO_ID,
            ARG_DARKSTORM_BACKUP_REPO_NAME,
        )?;
        let backup_repo: BlobRepo =
            args::open_repo_by_id(ctx.fb, &ctx.logger(), &matches, backup_repo_id).await?;

        scuba_sample.add("repo", backup_repo.get_repoid().id());
        scuba_sample.add("reponame", backup_repo.name().clone());

        Some(backup_repo)
    } else {
        scuba_sample.add("repo", repo_id.id());
        scuba_sample.add("reponame", repo_name.clone());
        None
    };

    let (repo, repo_parts) = {
        borrowed!(ctx);
        // FIXME: this cloned! will go away once HgRepo is asyncified
        cloned!(hg_repo_path);

        let (repo, preparer): (BlobRepo, BoxFuture<Result<Arc<BundlePreparer>, Error>>) = {
            if generate_bundles {
                let repo: InnerRepo = args::open_repo(ctx.fb, &ctx.logger(), &matches).await?;
                let filenode_verifier = match verify_lfs_blob_presence {
                    Some(uri) => {
                        let uri = uri.parse::<Uri>()?;
                        let verifier =
                            LfsVerifier::new(uri, Arc::new(repo.blob_repo.get_blobstore()))?;
                        FilenodeVerifier::LfsVerifier(verifier)
                    }
                    None => match maybe_darkstorm_backup_repo {
                        Some(ref backup_repo) => {
                            let verifier = DarkstormVerifier::new(
                                Arc::new(repo.blob_repo.get_blobstore()),
                                Arc::new(backup_repo.get_blobstore()),
                                backup_repo.filestore_config(),
                            );
                            FilenodeVerifier::DarkstormVerifier(verifier)
                        }
                        None => FilenodeVerifier::NoopVerifier,
                    },
                };
                (
                    repo.blob_repo.clone(),
                    BundlePreparer::new_generate_bundles(
                        repo,
                        base_retry_delay_ms,
                        retry_num,
                        lfs_params,
                        filenode_verifier,
                        bookmark_regex_force_lfs,
                        use_hg_server_bookmark_value_if_mismatch,
                        push_vars,
                    )
                    .map_ok(Arc::new)
                    .boxed(),
                )
            } else {
                let repo: BlobRepo = args::open_repo(ctx.fb, &ctx.logger(), &matches).await?;
                (
                    repo.clone(),
                    BundlePreparer::new_use_existing(
                        repo,
                        base_retry_delay_ms,
                        retry_num,
                        push_vars,
                    )
                    .map_ok(Arc::new)
                    .boxed(),
                )
            }
        };

        let overlay = {
            cloned!(repo);
            async move {
                let bookmarks = list_hg_server_bookmarks(hg_repo_path).await?;

                let bookmarks = stream::iter(bookmarks.into_iter())
                    .map(|(book, hg_cs_id)| {
                        cloned!(repo);
                        async move {
                            let maybe_bcs_id = repo
                                .bonsai_hg_mapping()
                                .get_bonsai_from_hg(ctx, hg_cs_id)
                                .await?;
                            Result::<_, Error>::Ok(maybe_bcs_id.map(|bcs_id| (book, bcs_id)))
                        }
                    })
                    .buffered(100)
                    .try_filter_map(|x| future::ready(Ok(x)))
                    .try_collect::<HashMap<_, _>>()
                    .await?;

                Ok(BookmarkOverlay::new(Arc::new(bookmarks)))
            }
        };

        let globalrevs_publishing_bookmark = repo_config
            .pushrebase
            .globalrevs_publishing_bookmark
            .as_ref();
        borrowed!(maybe_darkstorm_backup_repo);
        let globalrev_syncer = {
            cloned!(repo);
            async move {
                let globalrev_syncer = match globalrevs_publishing_bookmark {
                    Some(_) => {
                        if !generate_bundles {
                            return Err(format_err!(
                                "Syncing globalrevs ({}) requires generating bundles ({})",
                                HGSQL_GLOBALREVS_DB_ADDR,
                                GENERATE_BUNDLES
                            ));
                        }

                        match maybe_darkstorm_backup_repo {
                            Some(darkstorm_backup_repo) => {
                                Ok(GlobalrevSyncer::darkstorm(&repo, &darkstorm_backup_repo))
                            }
                            None => Ok(GlobalrevSyncer::Noop),
                        }
                    }
                    None => Ok(GlobalrevSyncer::Noop),
                };

                globalrev_syncer
            }
        };

        (repo, try_join3(preparer, overlay, globalrev_syncer))
    };

    let batch_size = args::get_usize(matches, "batch-size", DEFAULT_BATCH_SIZE);
    let single_bundle_timeout_ms = args::get_u64(
        matches,
        "single-bundle-timeout-ms",
        DEFAULT_SINGLE_BUNDLE_TIMEOUT_MS,
    );
    let verify_server_bookmark_on_failure = matches.is_present("verify-server-bookmark-on-failure");
    let hg_repo = hgrepo::HgRepo::new(
        hg_repo_path,
        batch_size,
        single_bundle_timeout_ms,
        verify_server_bookmark_on_failure,
    )?;

    let bookmarks = args::open_sql::<SqlBookmarksBuilder>(ctx.fb, config_store, &matches)?;

    let bookmarks = bookmarks.with_repo_id(repo_id);
    let reporting_handler = build_reporting_handler(&ctx, &scuba_sample, retry_num, &bookmarks);

    let (lock_via, unlock_via) = get_read_write_fetcher(
        ctx.fb,
        &mysql_options,
        get_repo_sqldb_address(&matches, &repo_config.hgsql_name)?.as_deref(),
        repo_config.hgsql_name.clone(),
        matches.is_present("lock-on-failure"),
        matches.is_present("repo-lock-sqlite"),
        readonly_storage.0,
    )?;

    match matches.subcommand() {
        (MODE_SYNC_ONCE, Some(sub_m)) => {
            let start_id = args::get_usize_opt(&sub_m, "start-id")
                .ok_or_else(|| Error::msg("--start-id must be specified"))?;

            let (maybe_log_entry, (bundle_preparer, mut overlay, globalrev_syncer)) = try_join(
                bookmarks
                    .read_next_bookmark_log_entries(
                        ctx.clone(),
                        start_id as u64,
                        1u64,
                        Freshness::MaybeStale,
                    )
                    .try_next(),
                repo_parts,
            )
            .await?;
            if let Some(log_entry) = maybe_log_entry {
                let (stats, res) = async {
                    let batches = bundle_preparer
                        .prepare_batches(&ctx, vec![log_entry.clone()])
                        .await?;
                    let mut combined_entries = bundle_preparer
                        .prepare_bundles(&ctx, batches, &mut overlay)
                        .await?;

                    let combined_entry = combined_entries.remove(0);
                    sync_single_combined_entry(
                        &ctx,
                        &combined_entry,
                        &hg_repo,
                        base_retry_delay_ms,
                        retry_num,
                        &globalrev_syncer,
                    )
                    .await
                }
                .timed()
                .await;

                let res = bind_sync_result(&[log_entry], res);
                let res = match res {
                    Ok(ok) => Ok((stats, ok)),
                    Err(err) => Err((Some(stats), err)),
                };
                let res = reporting_handler(res).await;
                let _ = build_outcome_handler(&ctx, &lock_via)(res).await?;
                Ok(())
            } else {
                info!(ctx.logger(), "no log entries found");
                Ok(())
            }
        }
        (MODE_SYNC_LOOP, Some(sub_m)) => {
            let start_id = args::get_i64_opt(&sub_m, "start-id");
            let bundle_buffer_size =
                args::get_usize_opt(&sub_m, "bundle-prefetch").unwrap_or(0) + 1;
            let combine_bundles = args::get_u64_opt(&sub_m, "combine-bundles").unwrap_or(1);
            let loop_forever = sub_m.is_present("loop-forever");
            let replayed_sync_counter =
                LatestReplayedSyncCounter::new(&repo, maybe_darkstorm_backup_repo.as_ref())?;
            let exit_path = sub_m
                .value_of("exit-file")
                .map(|name| Path::new(name).to_path_buf());

            // NOTE: We poll this callback twice:
            // - Once after possibly pulling a new piece of work.
            // - Once after pulling a prepared piece of work.
            //
            // This ensures that we exit ASAP in the two following cases:
            // - There is no work whatsoever. The first check exits early.
            // - There is a lot of buffered work. The 2nd check exits early without doing it all.
            borrowed!(ctx);
            let can_continue = move || match exit_path {
                Some(ref exit_path) if exit_path.exists() => {
                    info!(ctx.logger(), "path {:?} exists: exiting ...", exit_path);
                    false
                }
                _ => true,
            };

            let counter = replayed_sync_counter
                .get_counter(&ctx)
                .and_then(move |maybe_counter| {
                    future::ready(maybe_counter.or(start_id).ok_or_else(|| {
                        format_err!(
                            "{} counter not found. Pass `--start-id` flag to set the counter",
                            LATEST_REPLAYED_REQUEST_KEY
                        )
                    }))
                });

            let (start_id, (bundle_preparer, mut overlay, globalrev_syncer)) =
                try_join(counter, repo_parts).watched(ctx.logger()).await?;

            borrowed!(bundle_preparer: &BundlePreparer);
            let s = loop_over_log_entries(
                &ctx,
                &bookmarks,
                start_id,
                loop_forever,
                &scuba_sample,
                combine_bundles,
                &unlock_via,
            )
            .try_take_while({
                borrowed!(can_continue);
                move |_| future::ready(Ok(can_continue()))
            })
            .try_filter_map(|entry_vec| {
                if entry_vec.is_empty() {
                    future::ready(Ok(None))
                } else {
                    future::ready(Ok(Some(entry_vec)))
                }
            })
            .map(move |res_entries| async move {
                let entries = res_entries?;
                bundle_preparer
                    .prepare_batches(&ctx, entries)
                    .watched(ctx.logger())
                    .await
            })
            .buffered(bundle_buffer_size)
            .map_err(|cause| AnonymousError { cause })
            .map({
                let mut seen_first_batch = false;
                move |res_batches| {
                    let batches = res_batches?;
                    let mut batches = batches.into_iter();
                    let mut first = batches.next();
                    if !seen_first_batch {
                        // In case sync job failed to update "latest-replayed-request"
                        // counter during its previous run, the first batch might contain
                        // entries that were already synced to hg server. Syncing them again
                        // would result in an error. Let's try to detect this case and
                        // fix the first batch if possible.
                        if let Some(batch) = first {
                            first = maybe_adjust_batch(&ctx, batch, &overlay)
                                .map_err(|cause| AnonymousError { cause })?;
                            seen_first_batch = true;
                        }
                    }

                    let batches = first.into_iter().chain(batches);
                    Ok(bundle_preparer
                        .prepare_bundles(&ctx, batches.collect(), &mut overlay)
                        .watched(ctx.logger()))
                }
            })
            .map(|res| async move {
                let f = res?;
                f.watched(ctx.logger()).await
            })
            .buffered(bundle_buffer_size)
            .map_ok(|vec| stream::iter(vec.into_iter().map(Ok)))
            .try_flatten();

            let outcome_handler = build_outcome_handler(&ctx, &lock_via);
            pin_mut!(s);

            while let Some(res) = s.next().watched(ctx.logger()).await {
                if !can_continue() {
                    break;
                }

                let res = match res {
                    Ok(combined_entry) => {
                        let (stats, res) = sync_single_combined_entry(
                            &ctx,
                            &combined_entry,
                            &hg_repo,
                            base_retry_delay_ms,
                            retry_num,
                            &globalrev_syncer,
                        )
                        .watched(ctx.logger())
                        .timed()
                        .await;
                        let res = bind_sync_result(&combined_entry.components, res);

                        match res {
                            Ok(ok) => Ok((stats, ok)),
                            Err(err) => Err((Some(stats), err)),
                        }
                    }
                    Err(e) => Err((None, e)),
                };

                let res = reporting_handler(res).watched(ctx.logger()).await;
                let entry = outcome_handler(res).watched(ctx.logger()).await?;
                let next_id = get_id_to_search_after(&entry);

                retry(
                    &ctx.logger(),
                    |_| async {
                        let success = replayed_sync_counter
                            .set_counter(&ctx, next_id)
                            .watched(ctx.logger())
                            .await?;

                        if success {
                            Ok(())
                        } else {
                            bail!("failed to update counter")
                        }
                    },
                    base_retry_delay_ms,
                    retry_num,
                )
                .watched(ctx.logger())
                .await?;
            }
            Ok(())
        }
        _ => bail!("incorrect mode of operation is specified"),
    }
}

fn get_repo_sqldb_address<'a>(
    matches: &MononokeMatches<'a>,
    repo_name: &HgsqlName,
) -> Result<Option<String>, Error> {
    let config_store = matches.config_store();
    if let Some(db_addr) = matches.value_of("repo-lock-db-address") {
        return Ok(Some(db_addr.to_string()));
    }
    if !matches.is_present("lock-on-failure") {
        return Ok(None);
    }
    let handle = config_store.get_config_handle(CONFIGERATOR_HGSERVER_PATH.to_string())?;
    let config: Arc<ServerConfig> = handle.get();
    match config.sql_confs.get(AsRef::<str>::as_ref(repo_name)) {
        Some(sql_conf) => Ok(Some(sql_conf.db_tier.clone())),
        None => Ok(Some(config.sql_conf_default.db_tier.clone())),
    }
}

#[fbinit::main]
fn main(fb: FacebookInit) -> Result<()> {
    let app = args::MononokeAppBuilder::new("Mononoke -> hg sync job")
        .with_advanced_args_hidden()
        .with_fb303_args()
        .build()
        .arg(
            Arg::with_name("hg-repo-ssh-path")
                .takes_value(true)
                .required(true)
                .help("Remote path to hg repo to replay to. Example: ssh://hg.vip.facebook.com//data/scm/fbsource"),
        )
        .arg(
            Arg::with_name("log-to-scuba")
                .long("log-to-scuba")
                .takes_value(false)
                .required(false)
                .help("If set job will log individual bundle sync states to Scuba"),
        )
        .arg(
            Arg::with_name("lock-on-failure")
                .long("lock-on-failure")
                .takes_value(false)
                .required(false)
                .help("If set, mononoke repo will be locked on sync failure"),
        )
        .arg(
            Arg::with_name("base-retry-delay-ms")
                .long("base-retry-delay-ms")
                .takes_value(true)
                .required(false)
                .help("initial delay between failures. It will be increased on the successive attempts")
        )
        .arg(
            Arg::with_name("retry-num")
                .long("retry-num")
                .takes_value(true)
                .required(false)
                .help("how many times to retry to sync a single bundle")
        )
        .arg(
            Arg::with_name("batch-size")
                .long("batch-size")
                .takes_value(true)
                .required(false)
                .help("maximum number of bundles allowed over a single hg peer")
        )
        .arg(
            Arg::with_name("single-bundle-timeout-ms")
                .long("single-bundle-timeout-ms")
                .takes_value(true)
                .required(false)
                .help("a timeout to send a single bundle to (if exceeded, the peer is restarted)")
        )
        .arg(
            Arg::with_name("verify-server-bookmark-on-failure")
                .long("verify-server-bookmark-on-failure")
                .takes_value(false)
                .required(false)
                .help("if present, check after a failure whether a server bookmark is already in the expected location")
        )
        .arg(
            Arg::with_name("repo-lock-sqlite")
                .long("repo-lock-sqlite")
                .takes_value(false)
                .required(false)
                .help("Enable sqlite for repo_lock access, path is in repo-lock-db-address"),
        )
        .arg(
            Arg::with_name("repo-lock-db-address")
                .long("repo-lock-db-address")
                .takes_value(true)
                .required(false)
                .help("Db with repo_lock table. Will be used to lock/unlock repo"),
        )
        .arg(
            Arg::with_name(HGSQL_GLOBALREVS_USE_SQLITE)
                .long(HGSQL_GLOBALREVS_USE_SQLITE)
                .takes_value(false)
                .required(false)
                .help("Use sqlite for hgsql globalrev sync (use for testing)."),
        )
        .arg(
            Arg::with_name(HGSQL_GLOBALREVS_DB_ADDR)
                .long(HGSQL_GLOBALREVS_DB_ADDR)
                .takes_value(true)
                .required(false)
                .help("unused"),
        )
        .arg(
            Arg::with_name(GENERATE_BUNDLES)
                .long(GENERATE_BUNDLES)
                .takes_value(false)
                .required(false)
                .help("Generate new bundles instead of using bundles that were saved on Mononoke during push"),
        )
        .arg(
            Arg::with_name(ARG_BOOKMARK_REGEX_FORCE_GENERATE_LFS)
                .long(ARG_BOOKMARK_REGEX_FORCE_GENERATE_LFS)
                .takes_value(true)
                .required(false)
                .requires(GENERATE_BUNDLES)
                .help("force generation of lfs bundles for bookmarks that match regex"),
        )
        .arg(
            Arg::with_name("verify-lfs-blob-presence")
                .long("verify-lfs-blob-presence")
                .takes_value(true)
                .required(false)
                .help("If generating bundles, verify lfs blob presence at this batch endpoint"),
        )
        .arg(
            Arg::with_name(ARG_USE_HG_SERVER_BOOKMARK_VALUE_IF_MISMATCH)
                .long(ARG_USE_HG_SERVER_BOOKMARK_VALUE_IF_MISMATCH)
                .takes_value(false)
                .required(false)
                .requires(GENERATE_BUNDLES)
                .help("Every bundle generated by hg sync job tells hg server \
                'move bookmark BM from commit A to commit B' where commit A is the previous \
                value of the bookmark BM and commit B is the new value of the bookmark. \
                Sync job takes commit A from bookmark update log entry. \
                However it's possible that server's bookmark BM doesn't point to the same commit \
                as bookmark update log entry. \
                While usually it's a sign of problem in some cases it's an expected behaviour. \
                If this option is set let's allow sync job to take previous value of bookmark \
                from the server"),
        )
        .arg(
            Arg::with_name(ARG_BOOKMARK_MOVE_ANY_DIRECTION)
                .long(ARG_BOOKMARK_MOVE_ANY_DIRECTION)
                .takes_value(false)
                .required(false)
                .help("This flag controls whether we tell the server to allow \
                the bookmark movement in any direction (adding pushvar NON_FAST_FORWARD=true). \
                However, the server checks its per bookmark configuration before move."),
        )
        .arg(
            Arg::with_name(ARG_DARKSTORM_BACKUP_REPO_ID)
            .long(ARG_DARKSTORM_BACKUP_REPO_ID)
            .takes_value(true)
            .required(false)
            .help("Start hg-sync-job for syncing prod repo and darkstorm backup mononoke repo \
            and use darkstorm-backup-repo-id value as a target for sync."),
        )
        .arg(
            Arg::with_name(ARG_DARKSTORM_BACKUP_REPO_NAME)
            .long(ARG_DARKSTORM_BACKUP_REPO_NAME)
            .takes_value(true)
            .required(false)
            .help("Start hg-sync-job for syncing prod repo and darkstorm backup mononoke repo \
            and use darkstorm-backup-repo-name as a target for sync."),
        )
        .group(
            ArgGroup::with_name(ARG_DARKSTORM_BACKUP_REPO_GROUP)
                .args(&[ARG_DARKSTORM_BACKUP_REPO_ID, ARG_DARKSTORM_BACKUP_REPO_NAME])
        )
        .arg(
            Arg::with_name(ARG_BYPASS_READONLY)
                .long(ARG_BYPASS_READONLY)
                .takes_value(false)
                .required(false)
                .help("This flag make it possible to push bundle into readonly repos \
                (by adding pushvar BYPASS_READONLY=true)."),
        )
        .about(
            "Special job that takes bundles that were sent to Mononoke and \
             applies them to mercurial",
        );

    let sync_once = SubCommand::with_name(MODE_SYNC_ONCE)
        .about("Syncs a single bundle")
        .arg(
            Arg::with_name("start-id")
                .long("start-id")
                .takes_value(true)
                .required(true)
                .help("id in the database table to start sync with"),
        );
    let sync_loop = SubCommand::with_name(MODE_SYNC_LOOP)
        .about("Syncs bundles one by one")
        .arg(
            Arg::with_name("start-id")
                .long("start-id")
                .takes_value(true)
                .required(true)
                .help("if current counter is not set then `start-id` will be used"),
        )
        .arg(
            Arg::with_name("loop-forever")
                .long("loop-forever")
                .takes_value(false)
                .required(false)
                .help(
                    "If set job will loop forever even if there are no new entries in db or \
                     if there was an error",
                ),
        )
        .arg(
            Arg::with_name("bundle-prefetch")
                .long("bundle-prefetch")
                .takes_value(true)
                .required(false)
                .help("How many bundles to prefetch"),
        )
        .arg(
            Arg::with_name("exit-file")
                .long("exit-file")
                .takes_value(true)
                .required(false)
                .help(
                    "If you provide this argument, the sync loop will gracefully exit \
                     once this file exists",
                ),
        )
        .arg(
            Arg::with_name("combine-bundles")
                .long("combine-bundles")
                .takes_value(true)
                .required(false)
                .help("How many bundles to combine into a single bundle before sending to hg"),
        );
    let app = app.subcommand(sync_once).subcommand(sync_loop);

    let matches = app.get_matches(fb)?;
    let logger = matches.logger();

    let ctx = CoreContext::new_with_logger(fb, logger.clone());

    let fut = run(ctx, &matches);

    block_execute(
        fut,
        fb,
        "hg_sync_job",
        logger,
        &matches,
        cmdlib::monitoring::AliveService,
    )
}
