/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::{Arguments, Write};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use anyhow::{Context, Error};
use bytes::Bytes;
use cached_config::ConfigHandle;
use futures::{
    future,
    stream::{Stream, StreamExt, TryStreamExt},
};
use gotham::state::{FromState, State};
use gotham_derive::StateData;
use gotham_ext::{body_ext::BodyExt, middleware::ClientIdentity};
use http::{
    header::HeaderMap,
    uri::{Authority, Parts, PathAndQuery, Scheme, Uri},
};
use hyper::{header, Body, Request};
use permission_checker::{ArcPermissionChecker, MononokeIdentitySet};
use slog::Logger;
use tokio::runtime::Handle;

use blobrepo::BlobRepo;
use context::CoreContext;
use hyper::{client::HttpConnector, Client};
use hyper_openssl::HttpsConnector;
use lfs_protocol::{RequestBatch, RequestObject, ResponseBatch};
use metaconfig_types::RepoConfig;
use mononoke_types::ContentId;

use crate::config::ServerConfig;
use crate::errors::{ErrorKind, LfsServerContextErrorKind};
use crate::middleware::{LfsMethod, RequestContext};

pub type HttpsHyperClient = Client<HttpsConnector<HttpConnector>>;

// For some reason Source Control uses the read action to decide if a user can write to a repo...
const ACL_CHECK_ACTION: &str = "read";
// The user agent string presented to upstream
const CLIENT_USER_AGENT: &str = "mononoke-lfs-server/0.1.0 git/2.15.1";

struct LfsServerContextInner {
    repositories: HashMap<String, (BlobRepo, ArcPermissionChecker, RepoConfig)>,
    client: Arc<HttpsHyperClient>,
    server: Arc<ServerUris>,
    always_wait_for_upstream: bool,
    max_upload_size: Option<u64>,
    config_handle: ConfigHandle<ServerConfig>,
}

#[derive(Clone, StateData)]
pub struct LfsServerContext {
    inner: Arc<Mutex<LfsServerContextInner>>,
    will_exit: Arc<AtomicBool>,
}

impl LfsServerContext {
    pub fn new(
        repositories: HashMap<String, (BlobRepo, ArcPermissionChecker, RepoConfig)>,
        server: ServerUris,
        always_wait_for_upstream: bool,
        max_upload_size: Option<u64>,
        will_exit: Arc<AtomicBool>,
        config_handle: ConfigHandle<ServerConfig>,
    ) -> Result<Self, Error> {
        let connector = HttpsConnector::new()
            .map_err(Error::from)
            .context(ErrorKind::HttpClientInitializationFailed)?;
        let client = Client::builder().build(connector);

        let inner = LfsServerContextInner {
            repositories,
            server: Arc::new(server),
            client: Arc::new(client),
            always_wait_for_upstream,
            max_upload_size,
            config_handle,
        };

        Ok(LfsServerContext {
            inner: Arc::new(Mutex::new(inner)),
            will_exit,
        })
    }

    pub async fn request(
        &self,
        ctx: CoreContext,
        repository: String,
        identities: Option<&MononokeIdentitySet>,
        host: String,
    ) -> Result<RepositoryRequestContext, LfsServerContextErrorKind> {
        let (
            repo,
            aclchecker,
            client,
            server,
            always_wait_for_upstream,
            max_upload_size,
            config,
            enforce_acl_check,
        ) = {
            let inner = self.inner.lock().expect("poisoned lock");

            match inner.repositories.get(&repository) {
                Some((repo, aclchecker, repo_config)) => (
                    repo.clone(),
                    aclchecker.clone(),
                    inner.client.clone(),
                    inner.server.clone(),
                    inner.always_wait_for_upstream,
                    inner.max_upload_size,
                    inner.config_handle.get(),
                    repo_config.enforce_lfs_acl_check,
                ),
                None => {
                    return Err(LfsServerContextErrorKind::RepositoryDoesNotExist(
                        repository,
                    ));
                }
            }
        };

        let enforce_acl_check = enforce_acl_check && config.enforce_acl_check();
        let enforce_authentication = config.enforce_authentication();

        acl_check(
            aclchecker,
            identities,
            enforce_acl_check,
            enforce_authentication,
        )
        .await?;

        Ok(RepositoryRequestContext {
            ctx,
            repo,
            uri_builder: UriBuilder {
                repository,
                server,
                host,
            },
            client: HttpClient::Enabled(client),
            config,
            always_wait_for_upstream,
            max_upload_size,
        })
    }

    pub fn get_config_handle(&self) -> ConfigHandle<ServerConfig> {
        self.inner
            .lock()
            .expect("poisoned lock")
            .config_handle
            .clone()
    }

    pub fn get_config(&self) -> Arc<ServerConfig> {
        let inner = self.inner.lock().expect("poisoned lock");
        inner.config_handle.get()
    }

    pub fn will_exit(&self) -> bool {
        self.will_exit.load(Ordering::Relaxed)
    }
}

async fn acl_check(
    aclchecker: ArcPermissionChecker,
    identities: Option<&MononokeIdentitySet>,
    enforce_authorization: bool,
    enforce_authentication: bool,
) -> Result<(), LfsServerContextErrorKind> {
    let identities: Cow<MononokeIdentitySet> = match identities {
        Some(idents) => Cow::Borrowed(idents),
        None if enforce_authentication => {
            return Err(LfsServerContextErrorKind::NotAuthenticated);
        }
        None => Cow::Owned(MononokeIdentitySet::new()),
    };

    let acl_check = aclchecker
        .check_set(identities.as_ref(), &[ACL_CHECK_ACTION])
        .await
        .map_err(LfsServerContextErrorKind::PermissionCheckFailed)?;

    if !acl_check && enforce_authorization {
        return Err(LfsServerContextErrorKind::Forbidden.into());
    } else {
        return Ok(());
    }
}

#[derive(Clone)]
enum HttpClient {
    Enabled(Arc<HttpsHyperClient>),
    #[cfg(test)]
    Disabled,
}

#[derive(Clone)]
pub struct RepositoryRequestContext {
    pub ctx: CoreContext,
    pub repo: BlobRepo,
    pub uri_builder: UriBuilder,
    pub config: Arc<ServerConfig>,
    always_wait_for_upstream: bool,
    max_upload_size: Option<u64>,
    client: HttpClient,
}

pub struct HttpClientResponse<S: Stream<Item = Result<Bytes, Error>> + Send + 'static> {
    headers: HeaderMap,
    body: Option<S>,
    handle: Handle,
}

impl<S: Stream<Item = Result<Bytes, Error>> + Send + 'static> HttpClientResponse<S> {
    pub async fn concat(mut self) -> Result<Bytes, Error> {
        let body = self
            .body
            .take()
            .expect("Body cannot be missing")
            .try_concat_body(&self.headers)?
            .await?;
        Ok(body)
    }

    pub async fn discard(self) -> Result<(), Error> {
        self.concat().await?;
        Ok(())
    }

    pub fn into_inner(mut self) -> S {
        self.body.take().expect("Body cannot be missing")
    }
}

impl<S: Stream<Item = Result<Bytes, Error>> + Send + 'static> Drop for HttpClientResponse<S> {
    fn drop(&mut self) {
        if let Some(body) = self.body.take() {
            let discard = body.for_each(|_| future::ready(()));
            self.handle.spawn(discard);
        }
    }
}

fn host_maybe_port_to_host(host_maybe_port: &str) -> Result<String, LfsServerContextErrorKind> {
    Ok(host_maybe_port
        .parse::<Uri>()
        .map_err(|_e| LfsServerContextErrorKind::MissingHostHeader)?
        .authority()
        .ok_or(LfsServerContextErrorKind::MissingHostHeader)?
        .host()
        .to_string())
}

fn get_host_header(headers: &Option<&HeaderMap>) -> Result<String, LfsServerContextErrorKind> {
    let host_maybe_port = headers
        .ok_or(LfsServerContextErrorKind::MissingHostHeader)?
        .get(http::header::HOST)
        .ok_or(LfsServerContextErrorKind::MissingHostHeader)?
        .to_str()
        .map_err(|_e| LfsServerContextErrorKind::MissingHostHeader)?;

    host_maybe_port_to_host(host_maybe_port)
}

impl RepositoryRequestContext {
    pub async fn instantiate(
        state: &mut State,
        repository: String,
        method: LfsMethod,
    ) -> Result<Self, LfsServerContextErrorKind> {
        let req_ctx = state.borrow_mut::<RequestContext>();
        req_ctx.set_request(repository.clone(), method);

        let ctx = req_ctx.ctx.clone();

        let identities = if let Some(client_ident) = state.try_borrow::<ClientIdentity>() {
            client_ident.identities().as_ref()
        } else {
            None
        };

        let headers = HeaderMap::try_borrow_from(state);
        let host = get_host_header(&headers)?;

        let lfs_ctx = LfsServerContext::borrow_from(&state);
        lfs_ctx.request(ctx, repository, identities, host).await
    }

    pub fn logger(&self) -> &Logger {
        self.ctx.logger()
    }

    pub fn always_wait_for_upstream(&self) -> bool {
        self.always_wait_for_upstream
    }

    pub fn max_upload_size(&self) -> Option<u64> {
        self.max_upload_size
    }

    pub async fn dispatch(
        &self,
        mut request: Request<Body>,
    ) -> Result<HttpClientResponse<impl Stream<Item = Result<Bytes, Error>>>, Error> {
        #[allow(clippy::infallible_destructuring_match)]
        let client = match self.client {
            HttpClient::Enabled(ref client) => client,
            #[cfg(test)]
            HttpClient::Disabled => panic!("HttpClient is disabled in test"),
        };

        request.headers_mut().insert(
            header::USER_AGENT,
            header::HeaderValue::from_static(CLIENT_USER_AGENT),
        );
        let res = client.request(request);

        // NOTE: We spawn the request on an executor because we'd like to read the response even if
        // we drop the future returned here. The reason for that is that if we don't read a
        // response, Hyper will not reuse the conneciton for its pool (which makes sense for the
        // general case: if your server is sending you 5GB of data and you drop the future, you
        // don't want to read all that later just to reuse a connection).
        let fut = async move {
            let res = res.await.context(ErrorKind::UpstreamDidNotRespond)?;

            let (head, body) = res.into_parts();

            if !head.status.is_success() {
                let body = body.try_concat_body(&head.headers)?.await?;
                return Err(ErrorKind::UpstreamError(
                    head.status,
                    String::from_utf8_lossy(&body).to_string(),
                )
                .into());
            }

            // NOTE: This buffers the response here, since all our callsites need a concatenated
            // response. If we want to add callsites that need a streaming response, we should add
            // our own wrapper type that wraps the response and the headers.
            Ok(HttpClientResponse {
                headers: head.headers,
                body: Some(body.map_err(Error::from)),
                handle: Handle::current(),
            })
        };

        tokio::spawn(fut).await?
    }

    pub async fn upstream_batch(
        &self,
        batch: &RequestBatch,
    ) -> Result<Option<ResponseBatch>, ErrorKind> {
        let uri = match self.uri_builder.upstream_batch_uri()? {
            Some(uri) => uri,
            None => {
                return Ok(None);
            }
        };

        let body: Bytes = serde_json::to_vec(&batch)
            .map_err(|e| ErrorKind::SerializationFailed(e.into()))?
            .into();

        let req = Request::post(uri)
            .body(body.into())
            .map_err(|e| ErrorKind::Error(e.into()))?;

        let res = self
            .dispatch(req)
            .await
            .map_err(ErrorKind::UpstreamBatchNoResponse)?
            .concat()
            .await
            .map_err(ErrorKind::UpstreamBatchNoResponse)?;

        let batch = serde_json::from_slice::<ResponseBatch>(&res)
            .map_err(|e| ErrorKind::UpstreamBatchInvalid(e.into()))?;

        Ok(Some(batch))
    }
}

#[derive(Clone)]
pub struct UriBuilder {
    pub repository: String,
    pub server: Arc<ServerUris>,
    pub host: String,
}

impl UriBuilder {
    fn pick_uri(&self) -> Result<&BaseUri, ErrorKind> {
        Ok(self
            .server
            .self_uris
            .iter()
            .find(|&x| x.authority.host() == self.host)
            .ok_or_else(|| ErrorKind::HostNotAllowlisted(self.host.clone()))?)
    }

    pub fn upload_uri(&self, object: &RequestObject) -> Result<Uri, ErrorKind> {
        self.pick_uri()?
            .build(format_args!(
                "{}/upload/{}/{}",
                &self.repository, object.oid, object.size
            ))
            .map_err(|e| ErrorKind::UriBuilderFailed("upload_uri", e))
    }

    pub fn download_uri(&self, content_id: &ContentId) -> Result<Uri, ErrorKind> {
        self.pick_uri()?
            .build(format_args!("{}/download/{}", &self.repository, content_id))
            .map_err(|e| ErrorKind::UriBuilderFailed("download_uri", e))
    }

    pub fn consistent_download_uri(
        &self,
        content_id: &ContentId,
        routing_key: String,
    ) -> Result<Uri, ErrorKind> {
        self.pick_uri()?
            .build(format_args!(
                "{}/download/{}?routing={}",
                &self.repository, content_id, routing_key
            ))
            .map_err(|e| ErrorKind::UriBuilderFailed("consistent_download_uri", e))
    }

    pub fn upstream_batch_uri(&self) -> Result<Option<Uri>, ErrorKind> {
        self.server
            .upstream_uri
            .as_ref()
            .map(|uri| {
                uri.build(format_args!("objects/batch"))
                    .map_err(|e| ErrorKind::UriBuilderFailed("upstream_batch_uri", e))
            })
            .transpose()
    }
}

fn parse_and_check_uri(src: &str) -> Result<BaseUri, ErrorKind> {
    let uri = src
        .parse::<Uri>()
        .map_err(|_e| ErrorKind::InvalidUri(src.to_string(), "invalid uri"))?;

    let Parts {
        scheme,
        authority,
        path_and_query,
        ..
    } = uri.into_parts();

    Ok(BaseUri {
        scheme: scheme.ok_or_else(|| ErrorKind::InvalidUri(src.to_string(), "missing scheme"))?,
        authority: authority
            .ok_or_else(|| ErrorKind::InvalidUri(src.to_string(), "missing authority"))?,
        path_and_query,
    })
}

#[derive(Debug)]
pub struct ServerUris {
    /// The list of allowlisted root URLs to use when composing URLs for this LFS server
    self_uris: Vec<BaseUri>,
    /// The URL for an upstream LFS server
    upstream_uri: Option<BaseUri>,
}

impl ServerUris {
    pub fn new<'a>(self_uris: Vec<String>, upstream_uri: Option<String>) -> Result<Self, Error> {
        Ok(Self {
            self_uris: self_uris
                .into_iter()
                .map(|v| parse_and_check_uri(&v))
                .collect::<Result<Vec<_>, _>>()?,
            upstream_uri: upstream_uri.map(|v| parse_and_check_uri(&v)).transpose()?,
        })
    }
}

#[derive(Debug)]
struct BaseUri {
    scheme: Scheme,
    authority: Authority,
    path_and_query: Option<PathAndQuery>,
}

impl BaseUri {
    pub fn build(&self, args: Arguments) -> Result<Uri, Error> {
        let mut p = String::new();
        if let Some(ref path_and_query) = self.path_and_query {
            write!(&mut p, "{}", path_and_query)?;
            if !path_and_query.path().ends_with("/") {
                write!(&mut p, "{}", "/")?;
            }
        }
        p.write_fmt(args)?;

        Uri::builder()
            .scheme(self.scheme.clone())
            .authority(self.authority.clone())
            .path_and_query(&p[..])
            .build()
            .map_err(Error::from)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use anyhow::anyhow;
    use fbinit::FacebookInit;
    use lfs_protocol::Sha256 as LfsSha256;
    use mononoke_types::{hash::Sha256, ContentId};
    use permission_checker::PermissionCheckerBuilder;
    use std::str::FromStr;
    use test_repo_factory::TestRepoFactory;

    const ONES_HASH: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const TWOS_HASH: &str = "2222222222222222222222222222222222222222222222222222222222222222";
    const SIZE: u64 = 123;

    pub fn uri_builder(
        self_uris: Vec<&str>,
        upstream_uri: Option<&str>,
        host: String,
    ) -> Result<UriBuilder, Error> {
        let server = ServerUris::new(
            self_uris.into_iter().map(|v| v.to_string()).collect(),
            upstream_uri.map(|v| v.to_string()),
        )?;
        Ok(UriBuilder {
            repository: "repo123".to_string(),
            server: Arc::new(server),
            host,
        })
    }

    pub struct TestContextBuilder<'a> {
        fb: FacebookInit,
        repo: BlobRepo,
        self_uris: Vec<&'a str>,
        upstream_uri: Option<String>,
        config: ServerConfig,
        host: String,
    }

    impl TestContextBuilder<'_> {
        pub fn repo(mut self, repo: BlobRepo) -> Self {
            self.repo = repo;
            self
        }

        pub fn upstream_uri(mut self, upstream_uri: Option<String>) -> Self {
            self.upstream_uri = upstream_uri;
            self
        }

        pub fn config(mut self, config: ServerConfig) -> Self {
            self.config = config;
            self
        }

        pub fn build(self) -> Result<RepositoryRequestContext, Error> {
            let Self {
                fb,
                repo,
                self_uris,
                upstream_uri,
                config,
                host,
            } = self;

            let uri_builder = uri_builder(self_uris, upstream_uri.as_deref(), host)?;

            Ok(RepositoryRequestContext {
                ctx: CoreContext::test_mock(fb),
                repo,
                config: Arc::new(config),
                uri_builder,
                always_wait_for_upstream: false,
                max_upload_size: None,
                client: HttpClient::Disabled,
            })
        }
    }

    impl RepositoryRequestContext {
        pub fn test_builder(fb: FacebookInit) -> Result<TestContextBuilder<'static>, Error> {
            let repo = TestRepoFactory::new(fb)?.build()?;

            Self::test_builder_with_repo(fb, repo)
        }

        pub fn test_builder_with_repo(
            fb: FacebookInit,
            repo: BlobRepo,
        ) -> Result<TestContextBuilder<'static>, Error> {
            Ok(TestContextBuilder {
                fb,
                repo,
                self_uris: vec!["http://foo.com/"],
                upstream_uri: Some("http://bar.com".to_string()),
                config: ServerConfig::default(),
                host: "foo.com".to_string(),
            })
        }
    }

    fn obj() -> Result<RequestObject, Error> {
        Ok(RequestObject {
            oid: LfsSha256::from_str(ONES_HASH)?,
            size: SIZE,
        })
    }

    fn content_id() -> Result<ContentId, Error> {
        Ok(ContentId::from_str(ONES_HASH)?)
    }

    fn oid() -> Result<Sha256, Error> {
        Sha256::from_str(TWOS_HASH)
    }

    #[test]
    fn test_basic_upload_uri() -> Result<(), Error> {
        let b = uri_builder(
            vec!["http://foo.com"],
            Some("http://bar.com"),
            "foo.com".to_string(),
        )?;
        assert_eq!(
            b.upload_uri(&obj()?)?.to_string(),
            format!("http://foo.com/repo123/upload/{}/{}", ONES_HASH, SIZE),
        );
        Ok(())
    }

    #[test]
    fn test_basic_upload_uri_slash() -> Result<(), Error> {
        let b = uri_builder(
            vec!["http://foo.com/"],
            Some("http://bar.com"),
            "foo.com".to_string(),
        )?;
        assert_eq!(
            b.upload_uri(&obj()?)?.to_string(),
            format!("http://foo.com/repo123/upload/{}/{}", ONES_HASH, SIZE),
        );
        Ok(())
    }

    #[test]
    fn test_prefix_upload_uri() -> Result<(), Error> {
        let b = uri_builder(
            vec!["http://foo.com/bar"],
            Some("http://bar.com"),
            "foo.com".to_string(),
        )?;
        assert_eq!(
            b.upload_uri(&obj()?)?.to_string(),
            format!("http://foo.com/bar/repo123/upload/{}/{}", ONES_HASH, SIZE),
        );
        Ok(())
    }

    #[test]
    fn test_prefix_slash_upload_uri() -> Result<(), Error> {
        let b = uri_builder(
            vec!["http://foo.com/bar/"],
            Some("http://bar.com"),
            "foo.com".to_string(),
        )?;
        assert_eq!(
            b.upload_uri(&obj()?)?.to_string(),
            format!("http://foo.com/bar/repo123/upload/{}/{}", ONES_HASH, SIZE),
        );
        Ok(())
    }

    #[test]
    fn test_basic_download_uri() -> Result<(), Error> {
        let b = uri_builder(
            vec!["http://foo.com"],
            Some("http://bar.com"),
            "foo.com".to_string(),
        )?;
        assert_eq!(
            b.download_uri(&content_id()?)?.to_string(),
            format!("http://foo.com/repo123/download/{}", ONES_HASH),
        );
        Ok(())
    }

    #[test]
    fn test_basic_download_uri_slash() -> Result<(), Error> {
        let b = uri_builder(
            vec!["http://foo.com/"],
            Some("http://bar.com"),
            "foo.com".to_string(),
        )?;
        assert_eq!(
            b.download_uri(&content_id()?)?.to_string(),
            format!("http://foo.com/repo123/download/{}", ONES_HASH),
        );
        Ok(())
    }

    #[test]
    fn test_prefix_download_uri() -> Result<(), Error> {
        let b = uri_builder(
            vec!["http://foo.com/bar"],
            Some("http://bar.com"),
            "foo.com".to_string(),
        )?;
        assert_eq!(
            b.download_uri(&content_id()?)?.to_string(),
            format!("http://foo.com/bar/repo123/download/{}", ONES_HASH),
        );
        Ok(())
    }

    #[test]
    fn test_prefix_slash_download_uri() -> Result<(), Error> {
        let b = uri_builder(
            vec!["http://foo.com/bar/"],
            Some("http://bar.com"),
            "foo.com".to_string(),
        )?;
        assert_eq!(
            b.download_uri(&content_id()?)?.to_string(),
            format!("http://foo.com/bar/repo123/download/{}", ONES_HASH),
        );
        Ok(())
    }

    #[test]
    fn test_basic_consistent_download_uri() -> Result<(), Error> {
        let b = uri_builder(
            vec!["http://foo.com"],
            Some("http://bar.com"),
            "foo.com".to_string(),
        )?;
        assert_eq!(
            b.consistent_download_uri(&content_id()?, format!("{}", oid()?))?
                .to_string(),
            format!(
                "http://foo.com/repo123/download/{}?routing={}",
                ONES_HASH, TWOS_HASH
            ),
        );
        Ok(())
    }

    #[test]
    fn test_basic_upstream_batch_uri() -> Result<(), Error> {
        let b = uri_builder(
            vec!["http://foo.com"],
            Some("http://bar.com"),
            "foo.com".to_string(),
        )?;
        assert_eq!(
            b.upstream_batch_uri()?.map(|uri| uri.to_string()),
            Some(format!("http://bar.com/objects/batch")),
        );
        Ok(())
    }

    #[test]
    fn test_basic_upstream_batch_uri_slash() -> Result<(), Error> {
        let b = uri_builder(
            vec!["http://foo.com/"],
            Some("http://bar.com"),
            "foo.com".to_string(),
        )?;
        assert_eq!(
            b.upstream_batch_uri()?.map(|uri| uri.to_string()),
            Some(format!("http://bar.com/objects/batch")),
        );
        Ok(())
    }

    #[test]
    fn test_prefix_upstream_batch_uri() -> Result<(), Error> {
        let b = uri_builder(
            vec!["http://foo.com"],
            Some("http://bar.com/foo"),
            "foo.com".to_string(),
        )?;
        assert_eq!(
            b.upstream_batch_uri()?.map(|uri| uri.to_string()),
            Some(format!("http://bar.com/foo/objects/batch")),
        );
        Ok(())
    }

    #[test]
    fn test_prefix_slash_upstream_batch_uri() -> Result<(), Error> {
        let b = uri_builder(
            vec!["http://foo.com"],
            Some("http://bar.com/foo/"),
            "foo.com".to_string(),
        )?;
        assert_eq!(
            b.upstream_batch_uri()?.map(|uri| uri.to_string()),
            Some(format!("http://bar.com/foo/objects/batch")),
        );
        Ok(())
    }

    #[fbinit::test]
    async fn test_acl_check_no_certificates(_fb: FacebookInit) -> Result<(), Error> {
        let aclchecker = PermissionCheckerBuilder::always_allow().into();

        let res = acl_check(aclchecker, None, false, true).await;

        match res.err().unwrap() {
            LfsServerContextErrorKind::NotAuthenticated => Ok(()),
            _ => Err(anyhow!("test failed")),
        }
    }

    #[test]
    fn test_host_maybe_port_to_host() -> Result<(), Error> {
        assert_eq!(host_maybe_port_to_host("example.com")?, "example.com");
        assert_eq!(host_maybe_port_to_host("example.com:90")?, "example.com");
        assert_eq!(host_maybe_port_to_host("[::1]")?, "[::1]");
        assert_eq!(host_maybe_port_to_host("[::1]:80")?, "[::1]");
        assert_eq!(host_maybe_port_to_host("127.0.0.1:80")?, "127.0.0.1");
        Ok(())
    }
}
