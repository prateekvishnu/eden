/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Functions to load and parse Mononoke configuration.

use std::{
    collections::{HashMap, HashSet},
    path::Path,
    str,
};

use crate::convert::Convert;
use crate::errors::ConfigurationError;
use anyhow::{anyhow, Context, Result};
use cached_config::ConfigStore;
use metaconfig_types::{
    AllowlistEntry, BackupRepoConfig, BlobConfig, CensoredScubaParams, CommonConfig,
    HgsqlGlobalrevsName, HgsqlName, Redaction, RedactionConfig, RepoConfig, RepoReadOnly,
    StorageConfig,
};
use mononoke_types::RepositoryId;
use repos::{
    RawAclRegionConfig, RawCommonConfig, RawRepoConfig, RawRepoConfigs, RawRepoDefinition,
    RawStorageConfig,
};

const LIST_KEYS_PATTERNS_MAX_DEFAULT: u64 = 500_000;
const HOOK_MAX_FILE_SIZE_DEFAULT: u64 = 8 * 1024 * 1024; // 8MiB

/// Load configuration common to all repositories.
pub fn load_common_config(
    config_path: impl AsRef<Path>,
    config_store: &ConfigStore,
) -> Result<CommonConfig> {
    let RawRepoConfigs {
        common, storage, ..
    } = crate::raw::read_raw_configs(config_path.as_ref(), config_store)?;
    parse_common_config(common, &storage)
}

/// Holds configuration for repostories.
#[derive(Clone, Debug, PartialEq)]
pub struct RepoConfigs {
    /// Configs for all repositories
    pub repos: HashMap<String, RepoConfig>,
    /// Common configs for all repos
    pub common: CommonConfig,
}

/// Load configuration for repositories.
pub fn load_repo_configs(
    config_path: impl AsRef<Path>,
    config_store: &ConfigStore,
) -> Result<RepoConfigs> {
    let RawRepoConfigs {
        // TODO(stash): unused, can be deleted
        commit_sync: _,
        common,
        repos,
        storage,
        acl_region_configs,
        repo_definitions,
    } = crate::raw::read_raw_configs(config_path.as_ref(), config_store)?;
    let repo_definitions = repo_definitions.repo_definitions;
    let repo_configs = repos;
    let storage_configs = storage;

    let mut resolved_repo_configs = HashMap::new();
    let mut repoids = HashSet::new();

    for (reponame, raw_repo_definition) in repo_definitions.into_iter() {
        let repo_config = parse_with_repo_definition(
            raw_repo_definition,
            &repo_configs,
            &storage_configs,
            &acl_region_configs,
        )?;

        if !repoids.insert(repo_config.repoid) {
            return Err(ConfigurationError::DuplicatedRepoId(repo_config.repoid).into());
        }

        resolved_repo_configs.insert(reponame, repo_config);
    }

    let common = parse_common_config(common, &storage_configs)?;

    Ok(RepoConfigs {
        repos: resolved_repo_configs,
        common,
    })
}

fn parse_with_repo_definition(
    repo_definition: RawRepoDefinition,
    named_repo_configs: &HashMap<String, RawRepoConfig>,
    named_storage_configs: &HashMap<String, RawStorageConfig>,
    named_acl_region_configs: &HashMap<String, RawAclRegionConfig>,
) -> Result<RepoConfig> {
    let RawRepoDefinition {
        repo_id: repoid,
        backup_source_repo_name,
        repo_name,
        repo_config,
        write_lock_db_address,
        hipster_acl,
        enabled,
        readonly,
        needs_backup: _,
        external_repo_id: _,
        acl_region_config,
    } = repo_definition;

    let named_repo_config_name = repo_config
        .clone()
        .ok_or_else(|| ConfigurationError::InvalidConfig(format!("No named_repo_config")))?;

    let named_repo_config = named_repo_configs
        .get(named_repo_config_name.as_str())
        .ok_or_else(|| {
            ConfigurationError::InvalidConfig(format!(
                "no named_repo_config \"{}\" for repo \"{:?}\".",
                named_repo_config_name, repo_name
            ))
        })?
        .clone();

    let reponame = repo_name.ok_or_else(|| {
        ConfigurationError::InvalidConfig(format!("No repo_name in repo_definition"))
    })?;

    let backup_repo_config = if let Some(backup_source_repo_name) = backup_source_repo_name {
        if backup_source_repo_name != reponame {
            Some(BackupRepoConfig {
                source_repo_name: backup_source_repo_name,
            })
        } else {
            None
        }
    } else {
        None
    };

    let RawRepoConfig {
        storage_config,
        storage,
        bookmarks,
        hook_manager_params,
        hooks,
        redaction,
        generation_cache_size,
        scuba_table_hooks,
        cache_warmup,
        push,
        pushrebase,
        lfs,
        hash_validation_percentage,
        skiplist_index_blobstore_key,
        bundle2_replay_params,
        infinitepush,
        list_keys_patterns_max,
        filestore,
        hook_max_file_size,
        source_control_service,
        source_control_service_monitoring,
        derived_data_config,
        scuba_local_path_hooks,
        hgsql_name,
        hgsql_globalrevs_name,
        enforce_lfs_acl_check,
        repo_client_use_warm_bookmarks_cache,
        segmented_changelog_config,
        repo_client_knobs,
        phabricator_callsign,
        walker_config,
        cross_repo_commit_validation_config,
        ..
    } = named_repo_config;

    let named_storage_config = storage_config;

    let repoid = RepositoryId::new(repoid.context("missing repoid from configuration")?);

    let enabled = enabled.unwrap_or(true);

    let hooks: Vec<_> = hooks.unwrap_or_default().convert()?;

    let get_storage = move |name: &str| -> Result<StorageConfig> {
        let raw_storage_config = storage
            .as_ref()
            .and_then(|s| s.get(name))
            .or_else(|| named_storage_configs.get(name))
            .cloned()
            .ok_or_else(|| {
                ConfigurationError::InvalidConfig(format!("Storage \"{}\" not defined", name))
            })?;

        raw_storage_config.convert()
    };

    let storage_config = get_storage(
        &named_storage_config
            .ok_or_else(|| anyhow!("missing storage_config from configuration"))?,
    )?;

    let walker_config = walker_config.convert()?;

    let cache_warmup = cache_warmup.convert()?;

    let hook_manager_params = hook_manager_params.convert()?;

    let bookmarks = bookmarks.unwrap_or_default().convert()?;

    let push = push.convert()?.unwrap_or_default();

    let pushrebase = pushrebase.convert()?.unwrap_or_default();

    let bundle2_replay_params = bundle2_replay_params.convert()?.unwrap_or_default();

    let lfs = lfs.convert()?.unwrap_or_default();

    let hash_validation_percentage = hash_validation_percentage
        .map(|v| v.try_into())
        .transpose()?
        .unwrap_or(0);

    let readonly = if readonly.unwrap_or_default() {
        RepoReadOnly::ReadOnly("Set by config option".to_string())
    } else {
        RepoReadOnly::ReadWrite
    };

    let redaction = if redaction.unwrap_or(true) {
        Redaction::Enabled
    } else {
        Redaction::Disabled
    };

    let infinitepush = infinitepush.convert()?.unwrap_or_default();

    let generation_cache_size: usize = generation_cache_size
        .map(|v| v.try_into())
        .transpose()?
        .unwrap_or(10 * 1024 * 1024);

    let list_keys_patterns_max: u64 = list_keys_patterns_max
        .map(|v| v.try_into())
        .transpose()?
        .unwrap_or(LIST_KEYS_PATTERNS_MAX_DEFAULT);

    let hook_max_file_size: u64 = hook_max_file_size
        .map(|v| v.try_into())
        .transpose()?
        .unwrap_or(HOOK_MAX_FILE_SIZE_DEFAULT);

    let filestore = filestore.convert()?;

    let source_control_service = source_control_service.convert()?.unwrap_or_default();

    let source_control_service_monitoring = source_control_service_monitoring.convert()?;

    let derived_data_config = derived_data_config.convert()?.unwrap_or_default();

    // XXX only www has it explicitly specified.
    let hgsql_name = HgsqlName(hgsql_name.unwrap_or_else(|| reponame.to_string()));

    let hgsql_globalrevs_name =
        HgsqlGlobalrevsName(hgsql_globalrevs_name.unwrap_or_else(|| hgsql_name.0.clone()));

    let enforce_lfs_acl_check = enforce_lfs_acl_check.unwrap_or(false);
    let repo_client_use_warm_bookmarks_cache =
        repo_client_use_warm_bookmarks_cache.unwrap_or(false);

    let segmented_changelog_config = segmented_changelog_config.convert()?.unwrap_or_default();

    let repo_client_knobs = repo_client_knobs.convert()?.unwrap_or_default();

    let acl_region_config = acl_region_config
        .map(|key| {
            named_acl_region_configs.get(&key).cloned().ok_or_else(|| {
                ConfigurationError::InvalidConfig(format!(
                    "ACL region config \"{}\" not defined",
                    key
                ))
            })
        })
        .transpose()?
        .convert()?;

    let cross_repo_commit_validation_config = cross_repo_commit_validation_config.convert()?;

    Ok(RepoConfig {
        enabled,
        storage_config,
        generation_cache_size,
        repoid,
        scuba_table_hooks,
        scuba_local_path_hooks,
        cache_warmup,
        hook_manager_params,
        bookmarks,
        hooks,
        push,
        pushrebase,
        lfs,
        hash_validation_percentage,
        readonly,
        redaction,
        skiplist_index_blobstore_key,
        bundle2_replay_params,
        write_lock_db_address,
        infinitepush,
        list_keys_patterns_max,
        filestore,
        hook_max_file_size,
        hipster_acl,
        source_control_service,
        source_control_service_monitoring,
        derived_data_config,
        hgsql_name,
        hgsql_globalrevs_name,
        enforce_lfs_acl_check,
        repo_client_use_warm_bookmarks_cache,
        segmented_changelog_config,
        repo_client_knobs,
        phabricator_callsign,
        backup_repo_config,
        acl_region_config,
        walker_config,
        cross_repo_commit_validation_config,
    })
}

/// Holds configuration for storage.
#[derive(Debug, PartialEq)]
pub struct StorageConfigs {
    /// Configs for all storage
    pub storage: HashMap<String, StorageConfig>,
}

/// Load configuration for storage.
pub fn load_storage_configs(
    config_path: impl AsRef<Path>,
    config_store: &ConfigStore,
) -> Result<StorageConfigs> {
    let storage = crate::raw::read_raw_configs(config_path.as_ref(), config_store)?
        .storage
        .into_iter()
        .map(|(k, v)| Ok((k, v.convert()?)))
        .collect::<Result<_>>()?;

    Ok(StorageConfigs { storage })
}

fn parse_common_config(
    common: RawCommonConfig,
    common_storage_config: &HashMap<String, RawStorageConfig>,
) -> Result<CommonConfig> {
    let mut tiers_num = 0;
    let security_config: Vec<_> = common
        .whitelist_entry
        .unwrap_or_default()
        .into_iter()
        .map(|allowlist_entry| {
            let has_tier = allowlist_entry.tier.is_some();
            let has_identity = {
                if allowlist_entry.identity_data.is_none() ^ allowlist_entry.identity_type.is_none()
                {
                    return Err(ConfigurationError::InvalidFileStructure(
                        "identity type and data must be specified".into(),
                    )
                    .into());
                }

                allowlist_entry.identity_type.is_some()
            };

            if has_tier && has_identity {
                return Err(ConfigurationError::InvalidFileStructure(
                    "tier and identity cannot be both specified".into(),
                )
                .into());
            }

            if !has_tier && !has_identity {
                return Err(ConfigurationError::InvalidFileStructure(
                    "tier or identity must be specified".into(),
                )
                .into());
            }

            if allowlist_entry.tier.is_some() {
                tiers_num += 1;
                Ok(AllowlistEntry::Tier(allowlist_entry.tier.unwrap()))
            } else {
                let identity_type = allowlist_entry.identity_type.unwrap();

                Ok(AllowlistEntry::HardcodedIdentity {
                    ty: identity_type,
                    data: allowlist_entry.identity_data.unwrap(),
                })
            }
        })
        .collect::<Result<_>>()?;

    if tiers_num > 1 {
        return Err(
            ConfigurationError::InvalidFileStructure("only one tier is allowed".into()).into(),
        );
    }

    let loadlimiter_category = common
        .loadlimiter_category
        .filter(|category| !category.is_empty());
    let scuba_censored_table = common.scuba_censored_table;
    let scuba_censored_local_path = common.scuba_local_path_censored;

    let censored_scuba_params = CensoredScubaParams {
        table: scuba_censored_table,
        local_path: scuba_censored_local_path,
    };

    let get_blobstore = |name| -> Result<BlobConfig> {
        Ok(common_storage_config
            .get(name)
            .cloned()
            .ok_or_else(|| {
                ConfigurationError::InvalidConfig(format!(
                    "Storage \"{}\" not defined for redaction config",
                    name
                ))
            })?
            .convert()?
            .blobstore)
    };

    let redaction_config = common.redaction_config;
    let redaction_config = RedactionConfig {
        blobstore: get_blobstore(&redaction_config.blobstore)?,
        darkstorm_blobstore: match &redaction_config.darkstorm_blobstore {
            Some(storage) => Some(get_blobstore(storage)?),
            None => None,
        },
        redaction_sets_location: redaction_config.redaction_sets_location,
    };

    Ok(CommonConfig {
        security_config,
        loadlimiter_category,
        enable_http_control_api: common.enable_http_control_api,
        censored_scuba_params,
        redaction_config,
    })
}

impl RepoConfigs {
    /// Get individual `RepoConfig`, given a repo_id
    pub fn get_repo_config(&self, repo_id: RepositoryId) -> Option<(&String, &RepoConfig)> {
        self.repos
            .iter()
            .find(|(_, repo_config)| repo_config.repoid == repo_id)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use bookmarks_types::BookmarkName;
    use cached_config::TestSource;
    use maplit::{btreemap, hashmap, hashset};
    use metaconfig_types::{
        AclRegion, AclRegionConfig, AclRegionRule, BlameVersion, BlobConfig, BlobstoreId,
        BookmarkParams, BubbleDeletionMode, Bundle2ReplayParams, CacheWarmupParams,
        CommitSyncConfig, CommitSyncConfigVersion, CrossRepoCommitValidation, DatabaseConfig,
        DefaultSmallToLargeCommitSyncPathAction, DerivedDataConfig, DerivedDataTypesConfig,
        EphemeralBlobstoreConfig, FilestoreParams, HookBypass, HookConfig, HookManagerParams,
        HookParams, InfinitepushNamespace, InfinitepushParams, LfsParams, LocalDatabaseConfig,
        MetadataDatabaseConfig, MultiplexId, MultiplexedStoreType, PushParams, PushrebaseFlags,
        PushrebaseParams, RemoteDatabaseConfig, RemoteMetadataDatabaseConfig, RepoClientKnobs,
        SegmentedChangelogConfig, SegmentedChangelogHeadConfig, ShardableRemoteDatabaseConfig,
        ShardedRemoteDatabaseConfig, SmallRepoCommitSyncConfig, SourceControlServiceMonitoring,
        SourceControlServiceParams, UnodeVersion, WalkerConfig,
    };
    use mononoke_types::MPath;
    use mononoke_types_mocks::changesetid::ONES_CSID;
    use nonzero_ext::nonzero;
    use pretty_assertions::assert_eq;
    use regex::Regex;
    use repos::RawCommitSyncConfig;
    use std::fs::{create_dir_all, write};
    use std::num::NonZeroUsize;
    use std::sync::Arc;
    use std::time::Duration;
    use tempdir::TempDir;

    /// Parse a collection of raw commit sync config into commit sync config and validate it.
    fn parse_commit_sync_config(
        raw_commit_syncs: HashMap<String, RawCommitSyncConfig>,
    ) -> Result<HashMap<String, CommitSyncConfig>> {
        raw_commit_syncs
            .into_iter()
            .map(|(config_name, commit_sync_config)| {
                let commit_sync_config = commit_sync_config.convert()?;
                Ok((config_name, commit_sync_config))
            })
            .collect()
    }

    fn write_files(
        files: impl IntoIterator<Item = (impl AsRef<Path>, impl AsRef<[u8]>)>,
    ) -> TempDir {
        let tmp_dir = TempDir::new("mononoke_test_config").expect("tmp_dir failed");

        // Always create repos directory and repo_definitions directory
        create_dir_all(tmp_dir.path().join("repos")).expect("create repos failed");
        create_dir_all(tmp_dir.path().join("repo_definitions"))
            .expect("create repo_definitions failed");

        for (path, content) in files.into_iter() {
            let path = path.as_ref();
            let content = content.as_ref();

            let dir = path.parent().expect("missing parent");
            create_dir_all(tmp_dir.path().join(dir)).expect("create dir failed");
            write(tmp_dir.path().join(path), content).expect("write failed");
        }

        tmp_dir
    }

    #[test]
    fn test_commit_sync_config_correct() {
        let commit_sync_config = r#"
            [mega]
            large_repo_id = 1
            common_pushrebase_bookmarks = ["master"]
            version_name = "TEST_VERSION_NAME"

                [[mega.small_repos]]
                repoid = 2
                default_action = "preserve"
                bookmark_prefix = "repo2"
                direction = "small_to_large"

                    [mega.small_repos.mapping]
                    "p1" = ".r2-legacy/p1"
                    "p5" = ".r2-legacy/p5"

                [[mega.small_repos]]
                repoid = 3
                bookmark_prefix = "repo3"
                default_action = "prepend_prefix"
                default_prefix = "subdir"
                direction = "small_to_large"

                    [mega.small_repos.mapping]
                    "p1" = "p1"
                    "p4" = "p5/p4"
        "#;

        let paths = btreemap! {
            "common/commitsyncmap.toml" => commit_sync_config
        };
        let config_store = ConfigStore::new(Arc::new(TestSource::new()), None, None);
        let tmp_dir = write_files(&paths);
        let raw_config = crate::raw::read_raw_configs(tmp_dir.path(), &config_store)
            .expect("expect to read configs");
        let commit_sync = parse_commit_sync_config(raw_config.commit_sync)
            .expect("expected to get a commit sync config");

        let expected = hashmap! {
            "mega".to_owned() => CommitSyncConfig {
                large_repo_id: RepositoryId::new(1),
                common_pushrebase_bookmarks: vec![BookmarkName::new("master").unwrap()],
                small_repos: hashmap! {
                    RepositoryId::new(2) => SmallRepoCommitSyncConfig {
                        default_action: DefaultSmallToLargeCommitSyncPathAction::Preserve,
                        map: hashmap! {
                            MPath::new("p1").unwrap() => MPath::new(".r2-legacy/p1").unwrap(),
                            MPath::new("p5").unwrap() => MPath::new(".r2-legacy/p5").unwrap(),
                        },
                    },
                    RepositoryId::new(3) => SmallRepoCommitSyncConfig {
                        default_action: DefaultSmallToLargeCommitSyncPathAction::PrependPrefix(MPath::new("subdir").unwrap()),
                        map: hashmap! {
                            MPath::new("p1").unwrap() => MPath::new("p1").unwrap(),
                            MPath::new("p4").unwrap() => MPath::new("p5/p4").unwrap(),
                        },
                    }
                },
                version_name: CommitSyncConfigVersion("TEST_VERSION_NAME".to_string()),
            }
        };

        assert_eq!(commit_sync, expected);
    }

    #[test]
    fn test_commit_sync_config_large_is_small() {
        let commit_sync_config = r#"
            [mega]
            large_repo_id = 1
            common_pushrebase_bookmarks = ["master"]

                [[mega.small_repos]]
                repoid = 1
                bookmark_prefix = "repo2"
                default_action = "preserve"
                direction = "small_to_large"

                    [mega.small_repos.mapping]
                    "p1" = ".r2-legacy/p1"
                    "p5" = "subdir"
        "#;

        let paths = btreemap! {
            "common/commitsyncmap.toml" => commit_sync_config
        };
        let tmp_dir = write_files(&paths);
        let config_store = ConfigStore::new(Arc::new(TestSource::new()), None, None);
        let RawRepoConfigs { commit_sync, .. } =
            crate::raw::read_raw_configs(tmp_dir.path().as_ref(), &config_store).unwrap();
        for (_config_name, commit_sync_config) in commit_sync {
            let res = commit_sync_config.convert();
            let msg = format!("{:#?}", res);
            println!("res = {}", msg);
            assert!(res.is_err());
            assert!(msg.contains("is one of the small repos too"));
        }
    }

    #[test]
    fn test_commit_sync_config_duplicated_small_repos() {
        let commit_sync_config = r#"
            [mega]
            large_repo_id = 1
            common_pushrebase_bookmarks = ["master"]

                [[mega.small_repos]]
                repoid = 2
                bookmark_prefix = "repo2"
                default_action = "preserve"
                direction = "small_to_large"

                [[mega.small_repos]]
                repoid = 2
                bookmark_prefix = "repo3"
                default_action = "prepend_prefix"
                default_prefix = "subdir"
                direction = "small_to_large"
        "#;

        let paths = btreemap! {
            "common/commitsyncmap.toml" => commit_sync_config
        };
        let tmp_dir = write_files(&paths);
        let config_store = ConfigStore::new(Arc::new(TestSource::new()), None, None);
        let RawRepoConfigs { commit_sync, .. } =
            crate::raw::read_raw_configs(tmp_dir.path().as_ref(), &config_store).unwrap();
        for (_config_name, commit_sync_config) in commit_sync {
            let res = commit_sync_config.convert();
            let msg = format!("{:#?}", res);
            println!("res = {}", msg);
            assert!(res.is_err());
            assert!(msg.contains("present multiple times in the same CommitSyncConfig"));
        }
    }
    #[test]
    fn test_duplicated_repo_ids() {
        let www_content = r#"
            scuba_table_hooks="scm_hooks"
            storage_config="files"

            [storage.files.metadata.local]
            local_db_path = "/tmp/www"

            [storage.files.blobstore.blob_files]
            path = "/tmp/www"
        "#;
        let common_content = r#"
            loadlimiter_category="test-category"

            [[whitelist_entry]]
            tier = "tier1"

            [[whitelist_entry]]
            identity_type = "username"
            identity_data = "user"
        "#;

        let www1_repo_def = r#"
            repo_id=1
            repo_name="www1"
            repo_config="www1"
        "#;

        let www2_repo_def = r#"
            repo_id=1
            repo_name="www2"
            repo_config="www2"
        "#;

        let paths = btreemap! {
            "common/common.toml" => common_content,
            "common/commitsyncmap.toml" => "",
            "repos/www1/server.toml" => www_content,
            "repos/www2/server.toml" => www_content,
            "repo_definitions/www1/server.toml" => www1_repo_def,
            "repo_definitions/www2/server.toml" => www2_repo_def,
        };

        let config_store = ConfigStore::new(Arc::new(TestSource::new()), None, None);
        let tmp_dir = write_files(&paths);
        let res = load_repo_configs(tmp_dir.path(), &config_store);
        let msg = format!("{:#?}", res);
        println!("res = {}", msg);
        assert!(res.is_err());
        assert!(msg.contains("DuplicatedRepoId"));
    }

    #[test]
    fn test_read_manifest() {
        let fbsource_content = r#"
            generation_cache_size=1048576
            scuba_table_hooks="scm_hooks"
            skiplist_index_blobstore_key="skiplist_key"
            storage_config="main"
            list_keys_patterns_max=123
            hook_max_file_size=456
            repo_client_use_warm_bookmarks_cache=true
            phabricator_callsign="FBS"

            [cache_warmup]
            bookmark="master"
            commit_limit=100
            [hook_manager_params]
            disable_acl_checker=false
            all_hooks_bypassed=false
            bypassed_commits_scuba_table="commits_bypassed_hooks"

            [derived_data_config]
            enabled_config_name = "default"

            [derived_data_config.available_configs.default]
            types = ["fsnodes", "unodes", "blame"]
            unode_version = 2
            blame_filesize_limit = 101

            [[bookmarks]]
            name="master"
            allowed_users="^(svcscm|twsvcscm)$"

            [[bookmarks.hooks]]
            hook_name="hook1"

            [[bookmarks.hooks]]
            hook_name="rust:rusthook"

            [[bookmarks]]
            regex="[^/]*/stable"
            ensure_ancestor_of="master"
            allow_move_to_public_commits_without_hooks=true

            [[hooks]]
            name="hook1"
            bypass_commit_string="@allow_hook1"

            [[hooks]]
            name="rust:rusthook"
            config_ints={ int1 = 44 }
            config_ints_64={ int2 = 42 }
            [hooks.config_string_lists]
                list1 = ["val1", "val2"]

            [push]
            pure_push_allowed = false
            commit_scribe_category = "cat"

            [pushrebase]
            rewritedates = false
            recursion_limit = 1024
            forbid_p2_root_rebases = false
            casefolding_check = false
            emit_obsmarkers = false
            allow_change_xrepo_mapping_extra = true

            [lfs]
            threshold = 1000
            rollout_percentage = 56
            generate_lfs_blob_in_hg_sync_job = true

            [bundle2_replay_params]
            preserve_raw_bundle2 = true

            [infinitepush]
            allow_writes = true
            namespace_pattern = "foobar/.+"

            [filestore]
            chunk_size = 768
            concurrency = 48

            [source_control_service_monitoring]
            bookmarks_to_report_age= ["master", "master2"]

            [repo_client_knobs]
            allow_short_getpack_history = true

            [segmented_changelog_config]
            enabled = true
            master_bookmark = "test_bookmark"
            tailer_update_period_secs = 0
            skip_dag_load_at_startup = true
            reload_dag_save_period_secs = 0
            update_to_master_bookmark_period_secs = 120
            heads_to_include = [
                { bookmark = "test_bookmark" },
            ]
            extra_heads_to_include_in_background_jobs = []

            [backup_config]
            verification_enabled = false
            
            [walker_config]
            scrub_enabled = true
            validate_enabled = true
            
            [cross_repo_commit_validation_config]
            skip_bookmarks = ["weirdy"]
        "#;
        let fbsource_repo_def = r#"
            repo_id=0
            write_lock_db_address="write_lock_db_address"
            repo_name="fbsource"
            hipster_acl="foo/test"
            repo_config="fbsource"
            needs_backup=false
            backup_source_repo_name="source"
            acl_region_config="fbsource"
        "#;
        let www_content = r#"
            scuba_table_hooks="scm_hooks"
            storage_config="files"
            hgsql_name = "www-foobar"
            hgsql_globalrevs_name = "www-barfoo"
            phabricator_callsign="WWW"
            [segmented_changelog_config]
            heads_to_include = [
                { all_public_bookmarks_except = [] }
            ]
        "#;
        let www_repo_def = r#"
            repo_id=1
            repo_name="www"
            repo_config="www"
        "#;
        let common_content = r#"
            loadlimiter_category="test-category"
            scuba_censored_table="censored_table"
            scuba_local_path_censored="censored_local_path"

            [redaction_config]
            blobstore="main"
            redaction_sets_location="loc"

            [[whitelist_entry]]
            tier = "tier1"

            [[whitelist_entry]]
            identity_type = "username"
            identity_data = "user"
        "#;

        let storage = r#"
        [main.metadata.remote]
        primary = { db_address = "db_address" }
        filenodes = { sharded = { shard_map = "db_address_shards", shard_num = 123 } }
        mutation = { db_address = "mutation_db_address" }

        [main.blobstore.multiplexed]
        multiplex_id = 1
        scuba_table = "blobstore_scuba_table"
        multiplex_scuba_table = "multiplex_scuba_table"
        components = [
            { blobstore_id = 0, blobstore = { manifold = { manifold_bucket = "bucket" } } },
            { blobstore_id = 1, blobstore = { blob_files = { path = "/tmp/foo" } } },
        ]
        queue_db = { remote = { db_address = "queue_db_address" } }
        minimum_successful_writes = 2

        [files.metadata.local]
        local_db_path = "/tmp/www"

        [files.blobstore.blob_files]
        path = "/tmp/www"

        [files.ephemeral_blobstore]
        initial_bubble_lifespan_secs = 86400
        bubble_expiration_grace_secs = 3600
        bubble_deletion_mode = 1

        [files.ephemeral_blobstore.metadata.local]
        local_db_path = "/tmp/www-ephemeral"

        [files.ephemeral_blobstore.blobstore.blob_files]
        path = "/tmp/www-ephemeral"
        "#;

        let acl_region_configs = r#"
        [[fbsource.allow_rules]]
        name = "name_test"
        hipster_acl = "acl_test"
        [[fbsource.allow_rules.regions]]
        roots = ["1111111111111111111111111111111111111111111111111111111111111111"]
        heads = []
        path_prefixes = ["test/prefix", ""]
        "#;

        let paths = btreemap! {
            "common/storage.toml" => storage,
            "common/common.toml" => common_content,
            "common/commitsyncmap.toml" => "",
            "common/acl_regions.toml" => acl_region_configs,
            "repos/fbsource/server.toml" => fbsource_content,
            "repos/www/server.toml" => www_content,
            "repo_definitions/fbsource/server.toml" => fbsource_repo_def,
            "repo_definitions/www/server.toml" => www_repo_def,
            "my_path/my_files" => "",
        };

        let config_store = ConfigStore::new(Arc::new(TestSource::new()), None, None);
        let tmp_dir = write_files(&paths);
        let repoconfig =
            load_repo_configs(tmp_dir.path(), &config_store).expect("Read configs failed");

        let multiplex = BlobConfig::Multiplexed {
            multiplex_id: MultiplexId::new(1),
            scuba_table: Some("blobstore_scuba_table".to_string()),
            multiplex_scuba_table: Some("multiplex_scuba_table".to_string()),
            scuba_sample_rate: nonzero!(100u64),
            blobstores: vec![
                (
                    BlobstoreId::new(0),
                    MultiplexedStoreType::Normal,
                    BlobConfig::Manifold {
                        bucket: "bucket".into(),
                        prefix: "".into(),
                    },
                ),
                (
                    BlobstoreId::new(1),
                    MultiplexedStoreType::Normal,
                    BlobConfig::Files {
                        path: "/tmp/foo".into(),
                    },
                ),
            ],
            minimum_successful_writes: nonzero!(2usize),
            not_present_read_quorum: nonzero!(2usize),
            queue_db: DatabaseConfig::Remote(RemoteDatabaseConfig {
                db_address: "queue_db_address".into(),
            }),
        };
        let main_storage_config = StorageConfig {
            blobstore: multiplex,
            metadata: MetadataDatabaseConfig::Remote(RemoteMetadataDatabaseConfig {
                primary: RemoteDatabaseConfig {
                    db_address: "db_address".into(),
                },
                filenodes: ShardableRemoteDatabaseConfig::Sharded(ShardedRemoteDatabaseConfig {
                    shard_map: "db_address_shards".into(),
                    shard_num: NonZeroUsize::new(123).unwrap(),
                }),
                mutation: RemoteDatabaseConfig {
                    db_address: "mutation_db_address".into(),
                },
            }),
            ephemeral_blobstore: None,
        };

        let mut repos = HashMap::new();
        repos.insert(
            "fbsource".to_string(),
            RepoConfig {
                enabled: true,
                storage_config: main_storage_config.clone(),
                write_lock_db_address: Some("write_lock_db_address".into()),
                generation_cache_size: 1024 * 1024,
                repoid: RepositoryId::new(0),
                scuba_table_hooks: Some("scm_hooks".to_string()),
                scuba_local_path_hooks: None,
                cache_warmup: Some(CacheWarmupParams {
                    bookmark: BookmarkName::new("master").unwrap(),
                    commit_limit: 100,
                    microwave_preload: false,
                }),
                hook_manager_params: Some(HookManagerParams {
                    disable_acl_checker: false,
                    all_hooks_bypassed: false,
                    bypassed_commits_scuba_table: Some("commits_bypassed_hooks".to_string()),
                }),
                bookmarks: vec![
                    BookmarkParams {
                        bookmark: BookmarkName::new("master").unwrap().into(),
                        hooks: vec!["hook1".to_string(), "rust:rusthook".to_string()],
                        only_fast_forward: false,
                        allowed_users: Some(Regex::new("^(svcscm|twsvcscm)$").unwrap().into()),
                        allowed_hipster_group: None,
                        rewrite_dates: None,
                        hooks_skip_ancestors_of: vec![],
                        ensure_ancestor_of: None,
                        allow_move_to_public_commits_without_hooks: false,
                    },
                    BookmarkParams {
                        bookmark: Regex::new("[^/]*/stable").unwrap().into(),
                        hooks: vec![],
                        only_fast_forward: false,
                        allowed_users: None,
                        allowed_hipster_group: None,
                        rewrite_dates: None,
                        hooks_skip_ancestors_of: vec![],
                        ensure_ancestor_of: Some(BookmarkName::new("master").unwrap()),
                        allow_move_to_public_commits_without_hooks: true,
                    },
                ],
                hooks: vec![
                    HookParams {
                        name: "hook1".to_string(),
                        config: HookConfig {
                            bypass: Some(HookBypass::new_with_commit_msg("@allow_hook1".into())),
                            strings: hashmap! {},
                            ints: hashmap! {},
                            ints_64: hashmap! {},
                            string_lists: hashmap! {},
                            int_lists: hashmap! {},
                            int_64_lists: hashmap! {},
                        },
                    },
                    HookParams {
                        name: "rust:rusthook".to_string(),
                        config: HookConfig {
                            bypass: None,
                            strings: hashmap! {},
                            ints: hashmap! {
                                "int1".into() => 44,
                            },
                            ints_64: hashmap! {
                                "int2".into() => 42,
                            },
                            string_lists: hashmap! {
                                "list1".into() => vec!("val1".to_owned(), "val2".to_owned()),
                            },
                            int_lists: hashmap! {},
                            int_64_lists: hashmap! {},
                        },
                    },
                ],
                push: PushParams {
                    pure_push_allowed: false,
                    commit_scribe_category: Some("cat".to_string()),
                },
                pushrebase: PushrebaseParams {
                    flags: PushrebaseFlags {
                        rewritedates: false,
                        recursion_limit: Some(1024),
                        forbid_p2_root_rebases: false,
                        casefolding_check: false,
                        not_generated_filenodes_limit: 500,
                    },
                    block_merges: false,
                    emit_obsmarkers: false,
                    commit_scribe_category: None,
                    globalrevs_publishing_bookmark: None,
                    populate_git_mapping: false,
                    allow_change_xrepo_mapping_extra: true,
                },
                lfs: LfsParams {
                    threshold: Some(1000),
                    rollout_percentage: 56,
                    generate_lfs_blob_in_hg_sync_job: true,
                },
                hash_validation_percentage: 0,
                readonly: RepoReadOnly::ReadWrite,
                redaction: Redaction::Enabled,
                skiplist_index_blobstore_key: Some("skiplist_key".into()),
                bundle2_replay_params: Bundle2ReplayParams {
                    preserve_raw_bundle2: true,
                },
                infinitepush: InfinitepushParams {
                    allow_writes: true,
                    namespace: Some(InfinitepushNamespace::new(Regex::new("foobar/.+").unwrap())),
                    hydrate_getbundle_response: false,
                    commit_scribe_category: None,
                },
                list_keys_patterns_max: 123,
                hook_max_file_size: 456,
                filestore: Some(FilestoreParams {
                    chunk_size: 768,
                    concurrency: 48,
                }),
                hipster_acl: Some("foo/test".to_string()),
                source_control_service: SourceControlServiceParams {
                    permit_writes: false,
                    permit_service_writes: false,
                    service_write_hipster_acl: None,
                    permit_commits_without_parents: false,
                    service_write_restrictions: Default::default(),
                },
                source_control_service_monitoring: Some(SourceControlServiceMonitoring {
                    bookmarks_to_report_age: vec![
                        BookmarkName::new("master").unwrap(),
                        BookmarkName::new("master2").unwrap(),
                    ],
                }),
                derived_data_config: DerivedDataConfig {
                    enabled_config_name: "default".to_string(),
                    available_configs: hashmap!["default".to_string() => DerivedDataTypesConfig {
                        types: hashset! {
                            String::from("fsnodes"),
                            String::from("unodes"),
                            String::from("blame"),
                        },
                        mapping_key_prefixes: hashmap! {},
                        unode_version: UnodeVersion::V2,
                        blame_filesize_limit: Some(101),
                        hg_set_committer_extra: false,
                        blame_version: BlameVersion::V1,
                    },],
                    scuba_table: None,
                },
                hgsql_name: HgsqlName("fbsource".to_string()),
                hgsql_globalrevs_name: HgsqlGlobalrevsName("fbsource".to_string()),
                enforce_lfs_acl_check: false,
                repo_client_use_warm_bookmarks_cache: true,
                segmented_changelog_config: SegmentedChangelogConfig {
                    enabled: true,
                    tailer_update_period: None,
                    skip_dag_load_at_startup: true,
                    reload_dag_save_period: None,
                    update_to_master_bookmark_period: Some(Duration::from_secs(120)),
                    heads_to_include: vec![SegmentedChangelogHeadConfig::Bookmark(
                        BookmarkName::new("test_bookmark").unwrap(),
                    )],
                    extra_heads_to_include_in_background_jobs: vec![],
                },
                repo_client_knobs: RepoClientKnobs {
                    allow_short_getpack_history: true,
                },
                phabricator_callsign: Some("FBS".to_string()),
                backup_repo_config: Some(BackupRepoConfig {
                    source_repo_name: "source".to_string(),
                }),
                acl_region_config: Some(AclRegionConfig {
                    allow_rules: vec![AclRegionRule {
                        name: "name_test".to_string(),
                        regions: vec![AclRegion {
                            roots: vec![ONES_CSID],
                            heads: vec![],
                            path_prefixes: vec![Some(MPath::new("test/prefix").unwrap()), None],
                        }],
                        hipster_acl: "acl_test".to_string(),
                    }],
                }),
                walker_config: Some(WalkerConfig {
                    scrub_enabled: true,
                    validate_enabled: true,
                    params: None,
                }),
                cross_repo_commit_validation_config: Some(CrossRepoCommitValidation {
                    skip_bookmarks: [BookmarkName::new("weirdy").unwrap()].into(),
                }),
            },
        );

        repos.insert(
            "www".to_string(),
            RepoConfig {
                enabled: true,
                storage_config: StorageConfig {
                    metadata: MetadataDatabaseConfig::Local(LocalDatabaseConfig {
                        path: "/tmp/www".into(),
                    }),
                    blobstore: BlobConfig::Files {
                        path: "/tmp/www".into(),
                    },
                    ephemeral_blobstore: Some(EphemeralBlobstoreConfig {
                        blobstore: BlobConfig::Files {
                            path: "/tmp/www-ephemeral".into(),
                        },
                        metadata: DatabaseConfig::Local(LocalDatabaseConfig {
                            path: "/tmp/www-ephemeral".into(),
                        }),
                        initial_bubble_lifespan: Duration::from_secs(86400),
                        bubble_expiration_grace: Duration::from_secs(3600),
                        bubble_deletion_mode: BubbleDeletionMode::MarkOnly,
                    }),
                },
                write_lock_db_address: None,
                generation_cache_size: 10 * 1024 * 1024,
                repoid: RepositoryId::new(1),
                scuba_table_hooks: Some("scm_hooks".to_string()),
                scuba_local_path_hooks: None,
                cache_warmup: None,
                hook_manager_params: None,
                bookmarks: vec![],
                hooks: vec![],
                push: Default::default(),
                pushrebase: Default::default(),
                lfs: Default::default(),
                hash_validation_percentage: 0,
                readonly: RepoReadOnly::ReadWrite,
                redaction: Redaction::Enabled,
                skiplist_index_blobstore_key: None,
                bundle2_replay_params: Bundle2ReplayParams::default(),
                infinitepush: InfinitepushParams::default(),
                list_keys_patterns_max: LIST_KEYS_PATTERNS_MAX_DEFAULT,
                hook_max_file_size: HOOK_MAX_FILE_SIZE_DEFAULT,
                filestore: None,
                hipster_acl: None,
                source_control_service: SourceControlServiceParams::default(),
                source_control_service_monitoring: None,
                derived_data_config: DerivedDataConfig::default(),
                hgsql_name: HgsqlName("www-foobar".to_string()),
                hgsql_globalrevs_name: HgsqlGlobalrevsName("www-barfoo".to_string()),
                enforce_lfs_acl_check: false,
                repo_client_use_warm_bookmarks_cache: false,
                segmented_changelog_config: SegmentedChangelogConfig {
                    enabled: false,
                    tailer_update_period: Some(Duration::from_secs(45)),
                    skip_dag_load_at_startup: false,
                    reload_dag_save_period: Some(Duration::from_secs(3600)),
                    update_to_master_bookmark_period: Some(Duration::from_secs(60)),
                    heads_to_include: vec![SegmentedChangelogHeadConfig::AllPublicBookmarksExcept(
                        vec![],
                    )],
                    extra_heads_to_include_in_background_jobs: vec![],
                },
                repo_client_knobs: RepoClientKnobs::default(),
                phabricator_callsign: Some("WWW".to_string()),
                backup_repo_config: None,
                acl_region_config: None,
                walker_config: None,
                cross_repo_commit_validation_config: None,
            },
        );
        assert_eq!(
            repoconfig.common,
            CommonConfig {
                security_config: vec![
                    AllowlistEntry::Tier("tier1".to_string()),
                    AllowlistEntry::HardcodedIdentity {
                        ty: "username".to_string(),
                        data: "user".to_string(),
                    },
                ],
                loadlimiter_category: Some("test-category".to_string()),
                enable_http_control_api: false,
                censored_scuba_params: CensoredScubaParams {
                    table: Some("censored_table".to_string()),
                    local_path: Some("censored_local_path".to_string()),
                },
                redaction_config: RedactionConfig {
                    blobstore: main_storage_config.blobstore.clone(),
                    darkstorm_blobstore: None,
                    redaction_sets_location: "loc".to_string(),
                },
            }
        );
        assert_eq!(
            repoconfig.repos.get("www"),
            repos.get("www"),
            "www mismatch\ngot {:#?}\nwant {:#?}",
            repoconfig.repos.get("www"),
            repos.get("www")
        );
        assert_eq!(
            repoconfig.repos.get("fbsource"),
            repos.get("fbsource"),
            "fbsource mismatch\ngot {:#?}\nwant {:#?}",
            repoconfig.repos.get("fbsource"),
            repos.get("fbsource")
        );

        assert_eq!(
            &repoconfig.repos, &repos,
            "Repo mismatch:\n\
             got:\n\
             {:#?}\n\
             Want:\n\
             {:#?}",
            repoconfig.repos, repos
        )
    }

    #[test]
    fn test_broken_bypass_config() {
        // Incorrect bypass string
        let content = r#"
            storage_config = "sqlite"

            [storage.sqlite.metadata.local]
            local_db_path = "/tmp/fbsource"

            [storage.sqlite.blobstore.blob_files]
            path = "/tmp/fbsource"

            [[bookmarks]]
            name="master"
            [[bookmarks.hooks]]
            hook_name="hook1"
            [[hooks]]
            name="hook1"
            bypass_pushvar="var"
        "#;

        let content_def = r#"
            repo_id = 0
            repo_name = "fbsource"
            repo_config = "fbsource"
        "#;

        let paths = btreemap! {
            "common/commitsyncmap.toml" => "",
            "repos/fbsource/server.toml" => content,
            "repo_definitions/fbsource/server.toml" => content_def,
        };

        let config_store = ConfigStore::new(Arc::new(TestSource::new()), None, None);
        let tmp_dir = write_files(&paths);
        let res = load_repo_configs(tmp_dir.path(), &config_store);
        let msg = format!("{:#?}", res);
        println!("res = {}", msg);
        assert!(res.is_err());
        assert!(msg.contains("InvalidPushvar"));
    }

    #[test]
    fn test_broken_common_config() {
        fn check_fails(common: &str, expect: &str) {
            let content = r#"
                storage_config = "storage"

                [storage.storage.metadata.local]
                local_db_path = "/tmp/fbsource"

                [storage.storage.blobstore.blob_sqlite]
                path = "/tmp/fbsource"
            "#;

            let content_def = r#"
                repo_id = 0
                repo_name = "fbsource"
                repo_config = "fbsource"
            "#;

            let paths = btreemap! {
                "common/common.toml" => common,
                "common/commitsyncmap.toml" => "",
                "repos/fbsource/server.toml" => content,
                "repo_definitions/fbsource/server.toml" => content_def,
            };

            let config_store = ConfigStore::new(Arc::new(TestSource::new()), None, None);
            let tmp_dir = write_files(&paths);
            let res = load_repo_configs(tmp_dir.path(), &config_store);
            println!("res = {:?}", res);
            let msg = format!("{:?}", res);
            assert!(res.is_err(), "unexpected success for {}", common);
            assert!(
                msg.contains(expect),
                "wrong failure, wanted \"{}\" in {}",
                expect,
                common
            );
        }

        let common = r#"
        [[whitelist_entry]]
        identity_type="user"
        "#;
        check_fails(common, "identity type and data must be specified");

        let common = r#"
        [[whitelist_entry]]
        identity_data="user"
        "#;
        check_fails(common, "identity type and data must be specified");

        let common = r#"
        [[whitelist_entry]]
        tier="user"
        identity_type="user"
        identity_data="user"
        "#;
        check_fails(common, "tier and identity cannot be both specified");

        // Only one tier is allowed
        let common = r#"
        [[whitelist_entry]]
        tier="tier1"
        [[whitelist_entry]]
        tier="tier2"
        "#;
        check_fails(common, "only one tier is allowed");
    }

    #[test]
    fn test_common_storage() {
        const STORAGE: &str = r#"
        [multiplex_store.metadata.remote]
        primary = { db_address = "some_db" }
        filenodes = { sharded = { shard_map = "some-shards", shard_num = 123 } }
        mutation = { db_address = "some_db" }

        [multiplex_store.blobstore.multiplexed]
        multiplex_id = 1
        components = [
            { blobstore_id = 1, blobstore = { blob_files = { path = "/tmp/foo" } } },
        ]
        queue_db = { remote = { db_address = "queue_db_address" } }
        "#;

        const REPO: &str = r#"
        storage_config = "multiplex_store"

        # Not overriding common store
        [storage.some_other_store.metadata.remote]
        primary = { db_address = "other_db" }
        filenodes = { sharded = { shard_map = "other-shards", shard_num = 20 } }

        [storage.some_other_store.blobstore]
        disabled = {}
        "#;

        const REPO_DEF: &str = r#"
            repo_id = 123
            repo_config = "test"
            repo_name = "test"
        "#;

        const COMMON: &str = r#"
        [redaction_config]
        blobstore = "multiplex_store"
        redaction_sets_location = "loc"
        "#;

        let paths = btreemap! {
            "common/storage.toml" => STORAGE,
            "common/common.toml" => COMMON,
            "common/commitsyncmap.toml" => "",
            "repos/test/server.toml" => REPO,
            "repo_definitions/test/server.toml" => REPO_DEF,
        };

        let config_store = ConfigStore::new(Arc::new(TestSource::new()), None, None);
        let tmp_dir = write_files(&paths);
        let res = load_repo_configs(tmp_dir.path(), &config_store).expect("Read configs failed");

        let expected = hashmap! {
            "test".into() => RepoConfig {
                enabled: true,
                storage_config: StorageConfig {
                    blobstore: BlobConfig::Multiplexed {
                        multiplex_id: MultiplexId::new(1),
                        scuba_table: None,
                        multiplex_scuba_table: None,
                        scuba_sample_rate: nonzero!(100u64),
                        blobstores: vec![
                            (BlobstoreId::new(1), MultiplexedStoreType::Normal, BlobConfig::Files {
                                path: "/tmp/foo".into()
                            })
                        ],
                        minimum_successful_writes: nonzero!(1usize),
                        not_present_read_quorum: nonzero!(1usize),
                        queue_db: DatabaseConfig::Remote(
                            RemoteDatabaseConfig {
                                db_address: "queue_db_address".into(),
                            }
                        ),
                    },
                    metadata: MetadataDatabaseConfig::Remote(RemoteMetadataDatabaseConfig {
                        primary: RemoteDatabaseConfig {
                            db_address: "some_db".into(),
                        },
                        filenodes: ShardableRemoteDatabaseConfig::Sharded(ShardedRemoteDatabaseConfig {
                            shard_map: "some-shards".into(), shard_num: NonZeroUsize::new(123).unwrap()
                        }),
                        mutation: RemoteDatabaseConfig {
                            db_address: "some_db".into(),
                        },
                    }),
                    ephemeral_blobstore: None,
                },
                repoid: RepositoryId::new(123),
                generation_cache_size: 10 * 1024 * 1024,
                list_keys_patterns_max: LIST_KEYS_PATTERNS_MAX_DEFAULT,
                hook_max_file_size: HOOK_MAX_FILE_SIZE_DEFAULT,
                hgsql_name: HgsqlName("test".to_string()),
                hgsql_globalrevs_name: HgsqlGlobalrevsName("test".to_string()),
                ..Default::default()
            }
        };

        assert_eq!(
            res.repos, expected,
            "Got: {:#?}\nWant: {:#?}",
            &res.repos, expected
        )
    }

    #[test]
    fn test_common_blobstores_local_override() {
        const STORAGE: &str = r#"
        [multiplex_store.metadata.remote]
        primary = { db_address = "some_db" }
        filenodes = { sharded = { shard_map = "some-shards", shard_num = 123 } }

        [multiplex_store.blobstore.multiplexed]
        multiplex_id = 1
        components = [
            { blobstore_id = 1, blobstore = { blob_files = { path = "/tmp/foo" } } },
        ]
        queue_db = { remote = { db_address = "queue_db_address" } }

        [manifold_store.metadata.remote]
        primary = { db_address = "other_db" }
        filenodes = { sharded = { shard_map = "other-shards", shard_num = 456 } }
        mutation = { db_address = "other_mutation_db" }

        [manifold_store.blobstore.manifold]
        manifold_bucket = "bucketybucket"
        "#;

        const REPO: &str = r#"
        storage_config = "multiplex_store"

        # Override common store
        [storage.multiplex_store.metadata.remote]
        primary = { db_address = "other_other_db" }
        filenodes = { sharded = { shard_map = "other-other-shards", shard_num = 789 } }
        mutation = { db_address = "other_other_mutation_db" }

        [storage.multiplex_store.blobstore]
        disabled = {}
        "#;

        const REPO_DEF: &str = r#"
        repo_id = 123
        repo_config = "test"
        repo_name = "test"
        "#;

        const COMMON: &str = r#"
        [redaction_config]
        blobstore = "multiplex_store"
        redaction_sets_location = "loc"
        "#;

        let paths = btreemap! {
            "common/storage.toml" => STORAGE,
            "common/common.toml" => COMMON,
            "common/commitsyncmap.toml" => "",
            "repos/test/server.toml" => REPO,
            "repo_definitions/test/server.toml" => REPO_DEF,
        };

        let config_store = ConfigStore::new(Arc::new(TestSource::new()), None, None);
        let tmp_dir = write_files(&paths);
        let res = load_repo_configs(tmp_dir.path(), &config_store).expect("Read configs failed");

        let expected = hashmap! {
            "test".into() => RepoConfig {
                enabled: true,
                storage_config: StorageConfig {
                    blobstore: BlobConfig::Disabled,
                    metadata: MetadataDatabaseConfig::Remote( RemoteMetadataDatabaseConfig {
                        primary: RemoteDatabaseConfig { db_address: "other_other_db".into(), },
                        filenodes: ShardableRemoteDatabaseConfig::Sharded(ShardedRemoteDatabaseConfig { shard_map: "other-other-shards".into(), shard_num: NonZeroUsize::new(789).unwrap() }),
                        mutation: RemoteDatabaseConfig { db_address: "other_other_mutation_db".into(), },
                    }),

                    ephemeral_blobstore: None,
                },
                repoid: RepositoryId::new(123),
                generation_cache_size: 10 * 1024 * 1024,
                list_keys_patterns_max: LIST_KEYS_PATTERNS_MAX_DEFAULT,
                hook_max_file_size: HOOK_MAX_FILE_SIZE_DEFAULT,
                hgsql_name: HgsqlName("test".to_string()),
                hgsql_globalrevs_name: HgsqlGlobalrevsName("test".to_string()),
                ..Default::default()
            }
        };

        assert_eq!(
            res.repos, expected,
            "Got: {:#?}\nWant: {:#?}",
            &res.repos, expected
        )
    }

    #[test]
    fn test_stray_fields() {
        const REPO: &str = r#"
        storage_config = "randomstore"

        [storage.randomstore.metadata.remote]
        primary = { db_address = "other_other_db" }

        [storage.randomstore.blobstore.blob_files]
        path = "/tmp/foo"

        # Should be above
        readonly = true
        "#;

        const REPO_DEF: &str = r#"
         repo_id = 123
         readonly = true
         "#;

        let paths = btreemap! {
            "common/commitsyncmap.toml" => "",
            "repos/test/server.toml" => REPO,
            "repo_definitions/test/server.toml" => REPO_DEF,
        };

        let config_store = ConfigStore::new(Arc::new(TestSource::new()), None, None);
        let tmp_dir = write_files(&paths);
        let res = load_repo_configs(tmp_dir.path(), &config_store);
        let msg = format!("{:#?}", res);
        println!("res = {}", msg);
        assert!(res.is_err());
        assert!(msg.contains("unknown keys in config parsing"));
    }

    #[test]
    fn test_multiplexed_store_types() {
        const STORAGE: &str = r#"
        [multiplex_store.metadata.remote]
        primary = { db_address = "some_db" }
        filenodes = { sharded = { shard_map = "some-shards", shard_num = 123 } }

        [multiplex_store.blobstore.multiplexed]
        multiplex_id = 1
        components = [
            { blobstore_id = 1, blobstore = { blob_files = { path = "/tmp/foo1" } } },
            { blobstore_id = 2, store_type = { normal = {}}, blobstore = { blob_files = { path = "/tmp/foo2" } } },
            { blobstore_id = 3, store_type = { write_mostly = {}}, blobstore = { blob_files = { path = "/tmp/foo3" } } },
        ]
        queue_db = { remote = { db_address = "queue_db_address" } }
        "#;

        const REPO: &str = r#"
        storage_config = "multiplex_store"
        "#;

        const REPO_DEF: &str = r#"
        repo_id = 123
        repo_name = "test"
        repo_config = "test"
        "#;

        const COMMON: &str = r#"
        [redaction_config]
        blobstore = "multiplex_store"
        redaction_sets_location = "loc"
        "#;

        let paths = btreemap! {
            "common/storage.toml" => STORAGE,
            "common/common.toml" => COMMON,
            "common/commitsyncmap.toml" => "",
            "repos/test/server.toml" => REPO,
            "repo_definitions/test/server.toml" => REPO_DEF,
        };

        let config_store = ConfigStore::new(Arc::new(TestSource::new()), None, None);
        let tmp_dir = write_files(&paths);
        let res = load_repo_configs(tmp_dir.path(), &config_store).expect("Read configs failed");

        if let BlobConfig::Multiplexed { blobstores, .. } =
            &res.repos["test"].storage_config.blobstore
        {
            let expected_blobstores = vec![
                (
                    BlobstoreId::new(1),
                    MultiplexedStoreType::Normal,
                    BlobConfig::Files {
                        path: "/tmp/foo1".into(),
                    },
                ),
                (
                    BlobstoreId::new(2),
                    MultiplexedStoreType::Normal,
                    BlobConfig::Files {
                        path: "/tmp/foo2".into(),
                    },
                ),
                (
                    BlobstoreId::new(3),
                    MultiplexedStoreType::WriteMostly,
                    BlobConfig::Files {
                        path: "/tmp/foo3".into(),
                    },
                ),
            ];

            assert_eq!(
                blobstores, &expected_blobstores,
                "Blobstores parsed from config are wrong"
            );
        } else {
            panic!("Multiplexed config is not a multiplexed blobstore");
        }
    }
}
