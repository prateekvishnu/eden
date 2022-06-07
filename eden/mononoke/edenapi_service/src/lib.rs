/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![feature(associated_type_defaults)]
#![feature(try_blocks)]

mod context;
mod errors;
mod handlers;
mod middleware;
mod scuba;
mod utils;

use anyhow::Error;
use fbinit::FacebookInit;
use gotham::router::Router;
use gotham_ext::{
    handler::MononokeHttpHandler,
    middleware::{
        ClientIdentityMiddleware, LoadMiddleware, LogMiddleware, PostResponseMiddleware,
        ScubaMiddleware, ServerIdentityMiddleware, TimerMiddleware, TlsSessionDataMiddleware,
    },
};
use http::HeaderValue;
use mononoke_api::Mononoke;
use rate_limiting::RateLimitEnvironment;
use scuba_ext::MononokeScubaSampleBuilder;
use slog::Logger;
use std::path::Path;
use std::sync::{atomic::AtomicBool, Arc};

use crate::context::ServerContext;
use crate::handlers::build_router;
use crate::middleware::{OdsMiddleware, RequestContextMiddleware, RequestDumperMiddleware};
use crate::scuba::EdenApiScubaHandler;

pub type EdenApi = MononokeHttpHandler<Router>;

pub fn build(
    fb: FacebookInit,
    logger: Logger,
    mut scuba: MononokeScubaSampleBuilder,
    mononoke: Mononoke,
    will_exit: Arc<AtomicBool>,
    test_friendly_loging: bool,
    tls_session_data_log_path: Option<&Path>,
    rate_limiter: Option<RateLimitEnvironment>,
) -> Result<EdenApi, Error> {
    let ctx = ServerContext::new(mononoke, will_exit.clone());

    let log_middleware = if test_friendly_loging {
        LogMiddleware::test_friendly()
    } else {
        LogMiddleware::slog(logger.clone())
    };

    // Set up the router and handler for serving HTTP requests, along with custom middleware.
    // The middleware added here does not implement Gotham's usual Middleware trait; instead,
    // it uses the custom Middleware API defined in the gotham_ext crate. Native Gotham
    // middleware is set up during router setup in build_router.
    let router = build_router(ctx);

    let handler = MononokeHttpHandler::builder()
        .add(TlsSessionDataMiddleware::new(tls_session_data_log_path)?)
        .add(ClientIdentityMiddleware::new(fb, logger.clone()))
        .add(ServerIdentityMiddleware::new(HeaderValue::from_static(
            "edenapi_server",
        )))
        .add(PostResponseMiddleware::default())
        .add(RequestContextMiddleware::new(
            fb,
            logger,
            scuba.clone(),
            rate_limiter,
        ))
        .add(RequestDumperMiddleware::new(fb))
        .add(LoadMiddleware::new())
        .add(log_middleware)
        .add(OdsMiddleware::new())
        .add(<ScubaMiddleware<EdenApiScubaHandler>>::new({
            scuba.add("log_tag", "EdenAPI Request Processed");
            scuba
        }))
        .add(TimerMiddleware::new())
        .build(router);

    Ok(handler)
}
