/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::{Context, Error, Result};
use clap::Args;
use fbinit::FacebookInit;
use observability::ObservabilityContext;
use scuba_ext::MononokeScubaSampleBuilder;
use tunables::tunables;

/// Command line arguments that control scuba logging
#[derive(Args, Debug)]
pub struct ScubaLoggingArgs {
    /// The name of the scuba dataset to log to
    #[clap(long)]
    pub scuba_dataset: Option<String>,
    /// A log file to write JSON Scuba logs to (primarily useful in testing)
    #[clap(long)]
    pub scuba_log_file: Option<String>,
    /// Do not use the default scuba dataset for this app
    #[clap(long)]
    pub no_default_scuba_dataset: bool,
    /// Special dataset to be used by warm bookmark cache.  If a binary doesn't
    /// use warm bookmark cache then this parameter is ignored
    #[clap(long)]
    pub warm_bookmark_cache_scuba_dataset: Option<String>,
}

impl ScubaLoggingArgs {
    pub fn create_scuba_sample_builder(
        &self,
        fb: FacebookInit,
        observability_context: &ObservabilityContext,
        default_scuba_set: &Option<String>,
    ) -> Result<MononokeScubaSampleBuilder> {
        let mut scuba_logger = if let Some(scuba_dataset) = &self.scuba_dataset {
            MononokeScubaSampleBuilder::new(fb, scuba_dataset.as_str())
        } else if let Some(default_scuba_dataset) = default_scuba_set {
            if self.no_default_scuba_dataset {
                MononokeScubaSampleBuilder::with_discard()
            } else {
                MononokeScubaSampleBuilder::new(fb, default_scuba_dataset)
            }
        } else {
            MononokeScubaSampleBuilder::with_discard()
        };
        if let Some(scuba_log_file) = &self.scuba_log_file {
            scuba_logger = scuba_logger.with_log_file(scuba_log_file.clone())?;
        }
        let mut scuba_logger = scuba_logger
            .with_observability_context(observability_context.clone())
            .with_seq("seq");

        scuba_logger.add_common_server_data();

        Ok(scuba_logger)
    }

    pub fn create_warm_bookmark_cache_scuba_sample_builder(
        &self,
        fb: FacebookInit,
    ) -> Result<MononokeScubaSampleBuilder, Error> {
        let maybe_scuba = match self.warm_bookmark_cache_scuba_dataset.clone() {
            Some(scuba) => {
                let tw_task_id =
                    std::env::var("TW_TASK_ID").context("failed to get TW_TASK_ID env var")?;
                let tw_task_id: u32 = tw_task_id
                    .parse()
                    .context("failed to parse TW_TASK_ID env var")?;
                let mut sampling =
                    tunables().get_warm_bookmark_cache_loggin_tw_task_sampling() as u32;
                if sampling == 0 {
                    sampling = 10;
                }

                if tw_task_id % sampling == 0 {
                    Some(scuba)
                } else {
                    None
                }
            }
            None => None,
        };

        Ok(MononokeScubaSampleBuilder::with_opt_table(fb, maybe_scuba))
    }
}
