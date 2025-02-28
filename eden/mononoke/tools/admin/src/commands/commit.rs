/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

mod split;

use anyhow::{Context, Result};
use bonsai_git_mapping::BonsaiGitMapping;
use bonsai_globalrev_mapping::BonsaiGlobalrevMapping;
use bonsai_hg_mapping::BonsaiHgMapping;
use bonsai_svnrev_mapping::BonsaiSvnrevMapping;
use changesets::Changesets;
use clap::{Parser, Subcommand};
use mononoke_app::args::RepoArgs;
use mononoke_app::MononokeApp;
use repo_blobstore::RepoBlobstore;

use split::CommitSplitArgs;

/// Manipulate commits
#[derive(Parser)]
pub struct CommandArgs {
    #[clap(flatten)]
    repo_args: RepoArgs,

    #[clap(subcommand)]
    subcommand: CommitSubcommand,
}

#[facet::container]
pub struct Repo {
    #[facet]
    bonsai_hg_mapping: dyn BonsaiHgMapping,

    #[facet]
    bonsai_git_mapping: dyn BonsaiGitMapping,

    #[facet]
    bonsai_globalrev_mapping: dyn BonsaiGlobalrevMapping,

    #[facet]
    bonsai_svnrev_mapping: dyn BonsaiSvnrevMapping,

    #[facet]
    repo_blobstore: RepoBlobstore,

    #[facet]
    changesets: dyn Changesets,
}

#[derive(Subcommand)]
pub enum CommitSubcommand {
    /// Split a large commit into a stack
    ///
    /// Attempts to maintain limits on the number of files and size of all the
    /// files in each of the commits, however these limits are not strictly
    /// enforced, i.e. some of the commits might have larger sizes or more
    /// files, e.g. if a single file is larger than the limit, or if there are
    /// a large number of grouped copy sources and their destinations.
    ///
    /// The stack is printed in order from ancestor to descendant.
    Split(CommitSplitArgs),
}

pub async fn run(app: MononokeApp, args: CommandArgs) -> Result<()> {
    let ctx = app.new_context();

    let repo: Repo = app
        .open_repo(&args.repo_args)
        .await
        .context("Failed to open repo")?;

    match args.subcommand {
        CommitSubcommand::Split(split_args) => split::split(&ctx, &repo, split_args).await?,
    }

    Ok(())
}
