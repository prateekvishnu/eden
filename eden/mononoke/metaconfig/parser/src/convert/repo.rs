/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use bookmarks_types::BookmarkName;
use metaconfig_types::{
    BlameVersion, BookmarkOrRegex, BookmarkParams, Bundle2ReplayParams, CacheWarmupParams,
    ComparableRegex, DeletedManifestVersion, DerivedDataConfig, DerivedDataTypesConfig, HookBypass,
    HookConfig, HookManagerParams, HookParams, InfinitepushNamespace, InfinitepushParams,
    LfsParams, PushParams, PushrebaseFlags, PushrebaseParams, RepoClientKnobs,
    SegmentedChangelogConfig, SegmentedChangelogHeadConfig, ServiceWriteRestrictions,
    SourceControlServiceMonitoring, SourceControlServiceParams, UnodeVersion,
};
use mononoke_types::{ChangesetId, MPath, PrefixTrie};
use regex::Regex;
use repos::{
    RawBookmarkConfig, RawBundle2ReplayParams, RawCacheWarmupConfig, RawDerivedDataConfig,
    RawDerivedDataTypesConfig, RawHookConfig, RawHookManagerParams, RawInfinitepushParams,
    RawLfsParams, RawPushParams, RawPushrebaseParams, RawRepoClientKnobs,
    RawSegmentedChangelogConfig, RawSegmentedChangelogHeadConfig, RawServiceWriteRestrictions,
    RawSourceControlServiceMonitoring, RawSourceControlServiceParams,
};

use crate::convert::Convert;
use crate::errors::ConfigurationError;

impl Convert for RawCacheWarmupConfig {
    type Output = CacheWarmupParams;

    fn convert(self) -> Result<Self::Output> {
        Ok(CacheWarmupParams {
            bookmark: BookmarkName::new(self.bookmark)?,
            commit_limit: self
                .commit_limit
                .map(|v| v.try_into())
                .transpose()?
                .unwrap_or(200000),
            microwave_preload: self.microwave_preload.unwrap_or(false),
        })
    }
}

impl Convert for RawHookManagerParams {
    type Output = HookManagerParams;

    fn convert(self) -> Result<Self::Output> {
        Ok(HookManagerParams {
            disable_acl_checker: self.disable_acl_checker,
            all_hooks_bypassed: self.all_hooks_bypassed,
            bypassed_commits_scuba_table: self.bypassed_commits_scuba_table,
        })
    }
}

impl Convert for RawHookConfig {
    type Output = HookParams;

    fn convert(self) -> Result<Self::Output> {
        let bypass_commit_message = self.bypass_commit_string;

        let bypass_pushvar = self
            .bypass_pushvar
            .map(|s| {
                let parts: Vec<_> = s.split('=').collect();
                match parts.as_slice() {
                    [name, value] => Ok((name.to_string(), value.to_string())),
                    _ => Err(ConfigurationError::InvalidPushvar(s)),
                }
            })
            .transpose()?;

        let bypass = match (bypass_commit_message, bypass_pushvar) {
            (Some(msg), None) => Some(HookBypass::new_with_commit_msg(msg)),
            (None, Some((name, value))) => Some(HookBypass::new_with_pushvar(name, value)),
            (Some(msg), Some((name, value))) => Some(HookBypass::new_with_commit_msg_and_pushvar(
                msg, name, value,
            )),
            (None, None) => None,
        };

        let config = HookConfig {
            bypass,
            strings: self.config_strings.unwrap_or_default(),
            ints: self.config_ints.unwrap_or_default(),
            ints_64: self.config_ints_64.unwrap_or_default(),
            string_lists: self.config_string_lists.unwrap_or_default(),
            int_lists: self.config_int_lists.unwrap_or_default(),
            int_64_lists: self.config_int_64_lists.unwrap_or_default(),
        };

        Ok(HookParams {
            name: self.name,
            config,
        })
    }
}

impl Convert for RawBookmarkConfig {
    type Output = BookmarkParams;

    fn convert(self) -> Result<Self::Output> {
        let bookmark_or_regex = match (self.regex, self.name) {
            (None, Some(name)) => BookmarkOrRegex::Bookmark(BookmarkName::new(name).unwrap()),
            (Some(regex), None) => match Regex::new(&regex) {
                Ok(regex) => BookmarkOrRegex::Regex(ComparableRegex::new(regex)),
                Err(err) => {
                    return Err(ConfigurationError::InvalidConfig(format!(
                        "invalid bookmark regex: {}",
                        err
                    ))
                    .into());
                }
            },
            _ => {
                return Err(ConfigurationError::InvalidConfig(
                    "bookmark's params need to specify regex xor name".into(),
                )
                .into());
            }
        };

        let hooks = self.hooks.into_iter().map(|rbmh| rbmh.hook_name).collect();
        let only_fast_forward = self.only_fast_forward;
        let allowed_users = self
            .allowed_users
            .map(|re| Regex::new(&re))
            .transpose()?
            .map(ComparableRegex::new);
        let allowed_hipster_group = self.allowed_hipster_group;
        let rewrite_dates = self.rewrite_dates;
        let hooks_skip_ancestors_of = self
            .hooks_skip_ancestors_of
            .unwrap_or_default()
            .into_iter()
            .map(BookmarkName::new)
            .collect::<Result<Vec<_>, _>>()?;
        let ensure_ancestor_of = self.ensure_ancestor_of.map(BookmarkName::new).transpose()?;
        let allow_move_to_public_commits_without_hooks = self
            .allow_move_to_public_commits_without_hooks
            .unwrap_or(false);

        Ok(BookmarkParams {
            bookmark: bookmark_or_regex,
            hooks,
            only_fast_forward,
            allowed_users,
            allowed_hipster_group,
            rewrite_dates,
            hooks_skip_ancestors_of,
            ensure_ancestor_of,
            allow_move_to_public_commits_without_hooks,
        })
    }
}

impl Convert for RawPushParams {
    type Output = PushParams;

    fn convert(self) -> Result<Self::Output> {
        let default = PushParams::default();
        Ok(PushParams {
            pure_push_allowed: self.pure_push_allowed.unwrap_or(default.pure_push_allowed),
            commit_scribe_category: self.commit_scribe_category,
        })
    }
}

impl Convert for RawPushrebaseParams {
    type Output = PushrebaseParams;

    fn convert(self) -> Result<Self::Output> {
        let default = PushrebaseParams::default();
        Ok(PushrebaseParams {
            flags: PushrebaseFlags {
                rewritedates: self.rewritedates.unwrap_or(default.flags.rewritedates),
                recursion_limit: self
                    .recursion_limit
                    .map(|v| v.try_into())
                    .transpose()?
                    .or(default.flags.recursion_limit),
                forbid_p2_root_rebases: self
                    .forbid_p2_root_rebases
                    .unwrap_or(default.flags.forbid_p2_root_rebases),
                casefolding_check: self
                    .casefolding_check
                    .unwrap_or(default.flags.casefolding_check),
                not_generated_filenodes_limit: 500,
            },
            commit_scribe_category: self.commit_scribe_category,
            block_merges: self.block_merges.unwrap_or(default.block_merges),
            emit_obsmarkers: self.emit_obsmarkers.unwrap_or(default.emit_obsmarkers),
            globalrevs_publishing_bookmark: self
                .globalrevs_publishing_bookmark
                .map(BookmarkName::new)
                .transpose()?,
            populate_git_mapping: self
                .populate_git_mapping
                .unwrap_or(default.populate_git_mapping),
            allow_change_xrepo_mapping_extra: self
                .allow_change_xrepo_mapping_extra
                .unwrap_or(false),
        })
    }
}

impl Convert for RawBundle2ReplayParams {
    type Output = Bundle2ReplayParams;

    fn convert(self) -> Result<Self::Output> {
        Ok(Bundle2ReplayParams {
            preserve_raw_bundle2: self.preserve_raw_bundle2.unwrap_or(false),
        })
    }
}

impl Convert for RawLfsParams {
    type Output = LfsParams;

    fn convert(self) -> Result<Self::Output> {
        Ok(LfsParams {
            threshold: self.threshold.map(|v| v.try_into()).transpose()?,
            rollout_percentage: self.rollout_percentage.unwrap_or(0).try_into()?,
            generate_lfs_blob_in_hg_sync_job: self
                .generate_lfs_blob_in_hg_sync_job
                .unwrap_or(false),
        })
    }
}

impl Convert for RawInfinitepushParams {
    type Output = InfinitepushParams;

    fn convert(self) -> Result<Self::Output> {
        Ok(InfinitepushParams {
            allow_writes: self.allow_writes,
            namespace: self
                .namespace_pattern
                .and_then(|ns| Regex::new(&ns).ok().map(InfinitepushNamespace::new)),
            hydrate_getbundle_response: self.hydrate_getbundle_response.unwrap_or(false),
            commit_scribe_category: self.commit_scribe_category,
        })
    }
}

impl Convert for RawSourceControlServiceParams {
    type Output = SourceControlServiceParams;

    fn convert(self) -> Result<Self::Output> {
        let service_write_restrictions = self
            .service_write_restrictions
            .unwrap_or_default()
            .into_iter()
            .map(|(name, raw)| Ok((name, raw.convert()?)))
            .collect::<Result<HashMap<_, _>>>()?;

        Ok(SourceControlServiceParams {
            permit_writes: self.permit_writes,
            permit_service_writes: self.permit_service_writes,
            service_write_hipster_acl: self.service_write_hipster_acl,
            permit_commits_without_parents: self.permit_commits_without_parents,
            service_write_restrictions,
        })
    }
}

impl Convert for RawServiceWriteRestrictions {
    type Output = ServiceWriteRestrictions;

    fn convert(self) -> Result<Self::Output> {
        let RawServiceWriteRestrictions {
            permitted_methods,
            permitted_path_prefixes,
            permitted_bookmarks,
            permitted_bookmark_regex,
            ..
        } = self;

        let permitted_methods = permitted_methods.into_iter().collect();

        let permitted_path_prefixes = permitted_path_prefixes
            .map(|raw| {
                raw.into_iter()
                    .map(|path| MPath::new_opt(path.as_bytes()))
                    .collect::<Result<PrefixTrie>>()
            })
            .transpose()?
            .unwrap_or_default();

        let permitted_bookmarks = permitted_bookmarks
            .unwrap_or_default()
            .into_iter()
            .collect();

        let permitted_bookmark_regex = permitted_bookmark_regex
            .as_deref()
            .map(Regex::new)
            .transpose()
            .context("invalid service write permitted bookmark regex")?
            .map(ComparableRegex::new);

        Ok(ServiceWriteRestrictions {
            permitted_methods,
            permitted_path_prefixes,
            permitted_bookmarks,
            permitted_bookmark_regex,
        })
    }
}

impl Convert for RawSourceControlServiceMonitoring {
    type Output = SourceControlServiceMonitoring;

    fn convert(self) -> Result<Self::Output> {
        let bookmarks_to_report_age = self
            .bookmarks_to_report_age
            .into_iter()
            .map(BookmarkName::new)
            .collect::<Result<Vec<_>>>()?;
        Ok(SourceControlServiceMonitoring {
            bookmarks_to_report_age,
        })
    }
}

impl Convert for RawDerivedDataTypesConfig {
    type Output = DerivedDataTypesConfig;

    fn convert(self) -> Result<Self::Output> {
        let types = self.types.into_iter().collect();
        let mapping_key_prefixes = self.mapping_key_prefixes.into_iter().collect();
        let unode_version = match self.unode_version {
            None => UnodeVersion::default(),
            Some(1) => UnodeVersion::V1,
            Some(2) => UnodeVersion::V2,
            Some(version) => return Err(anyhow!("unknown unode version {}", version)),
        };
        let blame_filesize_limit = self.blame_filesize_limit.map(|limit| limit as u64);
        let blame_version = match self.blame_version {
            None => BlameVersion::default(),
            Some(1) => BlameVersion::V1,
            Some(2) => BlameVersion::V2,
            Some(version) => return Err(anyhow!("unknown blame version {}", version)),
        };
        let deleted_manifest_version = match self.deleted_manifest_version {
            None => DeletedManifestVersion::default(),
            Some(1) => DeletedManifestVersion::V1,
            Some(2) => DeletedManifestVersion::V2,
            Some(version) => return Err(anyhow!("unknown deleted manifest version {}", version)),
        };
        Ok(DerivedDataTypesConfig {
            types,
            mapping_key_prefixes,
            unode_version,
            blame_filesize_limit,
            hg_set_committer_extra: self.hg_set_committer_extra.unwrap_or(false),
            blame_version,
            deleted_manifest_version,
        })
    }
}

impl Convert for RawDerivedDataConfig {
    type Output = DerivedDataConfig;

    fn convert(self) -> Result<Self::Output> {
        Ok(DerivedDataConfig {
            scuba_table: self.scuba_table,
            enabled_config_name: self.enabled_config_name.unwrap_or_default(),
            available_configs: self
                .available_configs
                .unwrap_or_default()
                .into_iter()
                .map(|(s, raw_config)| Ok((s, raw_config.convert()?)))
                .collect::<Result<_, anyhow::Error>>()?,
        })
    }
}

impl Convert for RawRepoClientKnobs {
    type Output = RepoClientKnobs;

    fn convert(self) -> Result<Self::Output> {
        Ok(RepoClientKnobs {
            allow_short_getpack_history: self.allow_short_getpack_history,
        })
    }
}

impl Convert for RawSegmentedChangelogHeadConfig {
    type Output = SegmentedChangelogHeadConfig;

    fn convert(self) -> Result<Self::Output> {
        let res = match self {
            Self::all_public_bookmarks_except(exceptions) => {
                SegmentedChangelogHeadConfig::AllPublicBookmarksExcept(
                    exceptions
                        .into_iter()
                        .map(BookmarkName::new)
                        .collect::<Result<Vec<BookmarkName>>>()?,
                )
            }
            Self::bookmark(bookmark_name) => {
                SegmentedChangelogHeadConfig::Bookmark(BookmarkName::new(bookmark_name)?)
            }
            Self::bonsai_changeset(changeset_id) => {
                SegmentedChangelogHeadConfig::Changeset(ChangesetId::from_str(&changeset_id)?)
            }
            Self::UnknownField(_) => {
                return Err(anyhow!(
                    "Unknown variant of RawSegmentedChangelogHeadConfig!"
                ));
            }
        };
        Ok(res)
    }
}

impl Convert for RawSegmentedChangelogConfig {
    type Output = SegmentedChangelogConfig;

    fn convert(self) -> Result<Self::Output> {
        fn maybe_secs_to_duration(
            maybe_secs: Option<i64>,
            default: Option<Duration>,
        ) -> Result<Option<Duration>> {
            match maybe_secs {
                Some(secs) if secs == 0 => Ok(None),
                Some(secs) => Ok(Some(Duration::from_secs(secs.try_into()?))),
                None => Ok(default),
            }
        }

        let heads_to_include = self
            .heads_to_include
            .into_iter()
            .map(|raw| raw.convert())
            .collect::<Result<Vec<_>>>()?;

        let extra_heads_to_include_in_background_jobs = self
            .extra_heads_to_include_in_background_jobs
            .into_iter()
            .map(|raw| raw.convert())
            .collect::<Result<Vec<_>>>()?;

        let default = SegmentedChangelogConfig::default();
        Ok(SegmentedChangelogConfig {
            enabled: self.enabled.unwrap_or(default.enabled),
            tailer_update_period: maybe_secs_to_duration(
                self.tailer_update_period_secs,
                default.tailer_update_period,
            )?,
            skip_dag_load_at_startup: self
                .skip_dag_load_at_startup
                .unwrap_or(default.skip_dag_load_at_startup),
            reload_dag_save_period: maybe_secs_to_duration(
                self.reload_dag_save_period_secs,
                default.reload_dag_save_period,
            )?,
            update_to_master_bookmark_period: maybe_secs_to_duration(
                self.update_to_master_bookmark_period_secs,
                default.update_to_master_bookmark_period,
            )?,
            heads_to_include,
            extra_heads_to_include_in_background_jobs,
        })
    }
}
