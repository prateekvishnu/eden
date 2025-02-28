/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::{Error, Result};
use changeset_fetcher::ArcChangesetFetcher;
use context::CoreContext;
use futures_old::stream::Stream;
use futures_old::{Async, Poll};
use mononoke_types::{ChangesetId, Generation};
use std::collections::HashSet;

use crate::setcommon::*;
use crate::BonsaiNodeStream;

pub struct SetDifferenceNodeStream {
    keep_input: BonsaiInputStream,
    next_keep: Async<Option<(ChangesetId, Generation)>>,

    remove_input: BonsaiInputStream,
    next_remove: Async<Option<(ChangesetId, Generation)>>,

    remove_nodes: HashSet<ChangesetId>,
    remove_generation: Option<Generation>,
}

impl SetDifferenceNodeStream {
    pub fn new(
        ctx: CoreContext,
        changeset_fetcher: &ArcChangesetFetcher,
        keep_input: BonsaiNodeStream,
        remove_input: BonsaiNodeStream,
    ) -> SetDifferenceNodeStream {
        SetDifferenceNodeStream {
            keep_input: add_generations_by_bonsai(
                ctx.clone(),
                keep_input,
                changeset_fetcher.clone(),
            ),
            next_keep: Async::NotReady,
            remove_input: add_generations_by_bonsai(
                ctx.clone(),
                remove_input,
                changeset_fetcher.clone(),
            ),
            next_remove: Async::NotReady,
            remove_nodes: HashSet::new(),
            remove_generation: None,
        }
    }

    fn next_keep(&mut self) -> Result<&Async<Option<(ChangesetId, Generation)>>> {
        if self.next_keep.is_not_ready() {
            self.next_keep = self.keep_input.poll()?;
        }
        Ok(&self.next_keep)
    }

    fn next_remove(&mut self) -> Result<&Async<Option<(ChangesetId, Generation)>>> {
        if self.next_remove.is_not_ready() {
            self.next_remove = self.remove_input.poll()?;
        }
        Ok(&self.next_remove)
    }
}

impl Stream for SetDifferenceNodeStream {
    type Item = ChangesetId;
    type Error = Error;
    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        // This feels wrong, but in practice it's fine - it should be quick to hit a return, and
        // the standard futures_old::executor expects you to only return NotReady if blocked on I/O.
        loop {
            let (keep_hash, keep_gen) = match self.next_keep()? {
                &Async::NotReady => return Ok(Async::NotReady),
                &Async::Ready(None) => return Ok(Async::Ready(None)),
                &Async::Ready(Some((hash, gen))) => (hash, gen),
            };

            // Clear nodes that won't affect future results
            if self.remove_generation != Some(keep_gen) {
                self.remove_nodes.clear();
                self.remove_generation = Some(keep_gen);
            }

            // Gather the current generation's remove hashes
            loop {
                let remove_hash = match self.next_remove()? {
                    &Async::NotReady => return Ok(Async::NotReady),
                    &Async::Ready(Some((hash, gen))) if gen == keep_gen => hash,
                    &Async::Ready(Some((_, gen))) if gen > keep_gen => {
                        // Refers to a generation that's already past (probably nothing on keep
                        // side of this generation). Skip it.
                        self.next_remove = Async::NotReady;
                        continue;
                    }
                    _ => break, // Either no more or gen < keep_gen
                };
                self.remove_nodes.insert(remove_hash);
                self.next_remove = Async::NotReady; // will cause polling of remove_input
            }

            self.next_keep = Async::NotReady; // will cause polling of keep_input

            if !self.remove_nodes.contains(&keep_hash) {
                return Ok(Async::Ready(Some(keep_hash)));
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::errors::ErrorKind;
    use crate::fixtures::Linear;
    use crate::fixtures::MergeEven;
    use crate::fixtures::MergeUneven;
    use crate::fixtures::TestRepoFixture;
    use crate::setcommon::NotReadyEmptyStream;
    use crate::tests::get_single_bonsai_streams;
    use crate::tests::TestChangesetFetcher;
    use crate::UnionNodeStream;
    use changeset_fetcher::ArcChangesetFetcher;
    use context::CoreContext;
    use failure_ext::err_downcast;
    use fbinit::FacebookInit;
    use futures::{compat::Stream01CompatExt, stream::StreamExt as _};
    use futures_ext::StreamExt;
    use futures_old::executor::spawn;
    use revset_test_helper::assert_changesets_sequence;
    use revset_test_helper::{single_changeset_id, string_to_bonsai};
    use std::sync::Arc;

    #[fbinit::test]
    async fn difference_identical_node(fb: FacebookInit) {
        let ctx = CoreContext::test_mock(fb);
        let repo = Linear::getrepo(fb).await;
        let changeset_fetcher: ArcChangesetFetcher =
            Arc::new(TestChangesetFetcher::new(repo.clone()));
        let repo = Arc::new(repo);

        let hash = "a5ffa77602a066db7d5cfb9fb5823a0895717c5a";
        let changeset = string_to_bonsai(fb, &repo, hash).await;
        let nodestream = SetDifferenceNodeStream::new(
            ctx.clone(),
            &changeset_fetcher,
            single_changeset_id(ctx.clone(), changeset.clone(), &repo).boxify(),
            single_changeset_id(ctx.clone(), changeset.clone(), &repo).boxify(),
        )
        .boxify();
        assert_changesets_sequence(ctx.clone(), &repo, vec![], nodestream).await;
    }

    #[fbinit::test]
    async fn difference_node_and_empty(fb: FacebookInit) {
        let ctx = CoreContext::test_mock(fb);
        let repo = Linear::getrepo(fb).await;
        let changeset_fetcher: ArcChangesetFetcher =
            Arc::new(TestChangesetFetcher::new(repo.clone()));
        let repo = Arc::new(repo);

        let hash = "a5ffa77602a066db7d5cfb9fb5823a0895717c5a";
        let changeset = string_to_bonsai(fb, &repo, hash).await;
        let nodestream = SetDifferenceNodeStream::new(
            ctx.clone(),
            &changeset_fetcher,
            single_changeset_id(ctx.clone(), changeset.clone(), &repo).boxify(),
            NotReadyEmptyStream::new(0).boxify(),
        )
        .boxify();
        assert_changesets_sequence(ctx.clone(), &repo, vec![changeset], nodestream).await;
    }

    #[fbinit::test]
    async fn difference_empty_and_node(fb: FacebookInit) {
        let ctx = CoreContext::test_mock(fb);
        let repo = Linear::getrepo(fb).await;
        let changeset_fetcher: ArcChangesetFetcher =
            Arc::new(TestChangesetFetcher::new(repo.clone()));
        let repo = Arc::new(repo);

        let bcs_id = string_to_bonsai(fb, &repo, "a5ffa77602a066db7d5cfb9fb5823a0895717c5a").await;

        let nodestream = SetDifferenceNodeStream::new(
            ctx.clone(),
            &changeset_fetcher,
            NotReadyEmptyStream::new(0).boxify(),
            single_changeset_id(ctx.clone(), bcs_id, &repo).boxify(),
        )
        .boxify();

        assert_changesets_sequence(ctx.clone(), &repo, vec![], nodestream).await;
    }

    #[fbinit::test]
    async fn difference_two_nodes(fb: FacebookInit) {
        let ctx = CoreContext::test_mock(fb);
        let repo = Linear::getrepo(fb).await;
        let changeset_fetcher: ArcChangesetFetcher =
            Arc::new(TestChangesetFetcher::new(repo.clone()));
        let repo = Arc::new(repo);

        let bcs_id_1 = string_to_bonsai(
            fb,
            &repo.clone(),
            "d0a361e9022d226ae52f689667bd7d212a19cfe0",
        )
        .await;
        let bcs_id_2 = string_to_bonsai(
            fb,
            &repo.clone(),
            "3c15267ebf11807f3d772eb891272b911ec68759",
        )
        .await;
        let nodestream = SetDifferenceNodeStream::new(
            ctx.clone(),
            &changeset_fetcher,
            single_changeset_id(ctx.clone(), bcs_id_1.clone(), &repo).boxify(),
            single_changeset_id(ctx.clone(), bcs_id_2, &repo).boxify(),
        )
        .boxify();

        assert_changesets_sequence(ctx.clone(), &repo, vec![bcs_id_1], nodestream).await;
    }

    #[fbinit::test]
    async fn difference_error_node(fb: FacebookInit) {
        let ctx = CoreContext::test_mock(fb);
        let repo = Linear::getrepo(fb).await;
        let changeset_fetcher: ArcChangesetFetcher =
            Arc::new(TestChangesetFetcher::new(repo.clone()));
        let repo = Arc::new(repo);

        let hash = "a5ffa77602a066db7d5cfb9fb5823a0895717c5a";
        let changeset = string_to_bonsai(fb, &repo, hash).await;
        let mut nodestream = spawn(
            SetDifferenceNodeStream::new(
                ctx.clone(),
                &changeset_fetcher,
                RepoErrorStream {
                    item: changeset.clone(),
                }
                .boxify(),
                single_changeset_id(ctx.clone(), changeset, &repo).boxify(),
            )
            .boxify(),
        );

        match nodestream.wait_stream() {
            Some(Err(err)) => match err_downcast!(err, err: ErrorKind => err) {
                Ok(ErrorKind::RepoChangesetError(cs)) => assert_eq!(cs, changeset),
                Ok(bad) => panic!("unexpected error {:?}", bad),
                Err(bad) => panic!("unknown error {:?}", bad),
            },
            Some(Ok(bad)) => panic!("unexpected success {:?}", bad),
            None => panic!("no result"),
        };
    }

    #[fbinit::test]
    async fn slow_ready_difference_nothing(fb: FacebookInit) {
        // Tests that we handle an input staying at NotReady for a while without panicking
        let ctx = CoreContext::test_mock(fb);
        let repo = Linear::getrepo(fb).await;
        let changeset_fetcher: ArcChangesetFetcher = Arc::new(TestChangesetFetcher::new(repo));

        let mut nodestream = SetDifferenceNodeStream::new(
            ctx,
            &changeset_fetcher,
            NotReadyEmptyStream::new(10).boxify(),
            NotReadyEmptyStream::new(10).boxify(),
        )
        .compat();

        assert!(nodestream.next().await.is_none());
    }

    #[fbinit::test]
    async fn difference_union_with_single_node(fb: FacebookInit) {
        let ctx = CoreContext::test_mock(fb);
        let repo = Linear::getrepo(fb).await;
        let changeset_fetcher: ArcChangesetFetcher =
            Arc::new(TestChangesetFetcher::new(repo.clone()));
        let repo = Arc::new(repo);

        let inputs = get_single_bonsai_streams(
            ctx.clone(),
            &repo,
            &[
                "3c15267ebf11807f3d772eb891272b911ec68759",
                "a9473beb2eb03ddb1cccc3fbaeb8a4820f9cd157",
                "d0a361e9022d226ae52f689667bd7d212a19cfe0",
            ],
        )
        .await;

        let nodestream =
            UnionNodeStream::new(ctx.clone(), &changeset_fetcher, inputs.into_iter()).boxify();

        let bcs_id = string_to_bonsai(
            fb,
            &repo.clone(),
            "3c15267ebf11807f3d772eb891272b911ec68759",
        )
        .await;
        let nodestream = SetDifferenceNodeStream::new(
            ctx.clone(),
            &changeset_fetcher,
            nodestream,
            single_changeset_id(ctx.clone(), bcs_id, &repo).boxify(),
        )
        .boxify();

        assert_changesets_sequence(
            ctx.clone(),
            &repo,
            vec![
                string_to_bonsai(fb, &repo, "a9473beb2eb03ddb1cccc3fbaeb8a4820f9cd157").await,
                string_to_bonsai(fb, &repo, "d0a361e9022d226ae52f689667bd7d212a19cfe0").await,
            ],
            nodestream,
        )
        .await;
    }

    #[fbinit::test]
    async fn difference_single_node_with_union(fb: FacebookInit) {
        let ctx = CoreContext::test_mock(fb);
        let repo = Linear::getrepo(fb).await;
        let changeset_fetcher: ArcChangesetFetcher =
            Arc::new(TestChangesetFetcher::new(repo.clone()));
        let repo = Arc::new(repo);

        let inputs = get_single_bonsai_streams(
            ctx.clone(),
            &repo,
            &[
                "3c15267ebf11807f3d772eb891272b911ec68759",
                "a9473beb2eb03ddb1cccc3fbaeb8a4820f9cd157",
                "d0a361e9022d226ae52f689667bd7d212a19cfe0",
            ],
        )
        .await;
        let nodestream =
            UnionNodeStream::new(ctx.clone(), &changeset_fetcher, inputs.into_iter()).boxify();

        let bcs_id = string_to_bonsai(
            fb,
            &repo.clone(),
            "3c15267ebf11807f3d772eb891272b911ec68759",
        )
        .await;
        let nodestream = SetDifferenceNodeStream::new(
            ctx.clone(),
            &changeset_fetcher,
            single_changeset_id(ctx.clone(), bcs_id, &repo).boxify(),
            nodestream,
        )
        .boxify();

        assert_changesets_sequence(ctx.clone(), &repo, vec![], nodestream).await;
    }

    #[fbinit::test]
    async fn difference_merge_even(fb: FacebookInit) {
        let ctx = CoreContext::test_mock(fb);
        let repo = MergeEven::getrepo(fb).await;
        let changeset_fetcher: ArcChangesetFetcher =
            Arc::new(TestChangesetFetcher::new(repo.clone()));
        let repo = Arc::new(repo);

        // Top three commits in my hg log -G -r 'all()' output
        let inputs = get_single_bonsai_streams(
            ctx.clone(),
            &repo,
            &[
                "1f6bc010883e397abeca773192f3370558ee1320",
                "4f7f3fd428bec1a48f9314414b063c706d9c1aed",
                "16839021e338500b3cf7c9b871c8a07351697d68",
            ],
        )
        .await;

        let left_nodestream =
            UnionNodeStream::new(ctx.clone(), &changeset_fetcher, inputs.into_iter()).boxify();

        // Everything from base to just before merge on one side
        let inputs = get_single_bonsai_streams(
            ctx.clone(),
            &repo,
            &[
                "4f7f3fd428bec1a48f9314414b063c706d9c1aed",
                "b65231269f651cfe784fd1d97ef02a049a37b8a0",
                "d7542c9db7f4c77dab4b315edd328edf1514952f",
                "15c40d0abc36d47fb51c8eaec51ac7aad31f669c",
            ],
        )
        .await;
        let right_nodestream =
            UnionNodeStream::new(ctx.clone(), &changeset_fetcher, inputs.into_iter()).boxify();

        let nodestream = SetDifferenceNodeStream::new(
            ctx.clone(),
            &changeset_fetcher,
            left_nodestream,
            right_nodestream,
        )
        .boxify();

        assert_changesets_sequence(
            ctx.clone(),
            &repo,
            vec![
                string_to_bonsai(fb, &repo, "1f6bc010883e397abeca773192f3370558ee1320").await,
                string_to_bonsai(fb, &repo, "16839021e338500b3cf7c9b871c8a07351697d68").await,
            ],
            nodestream,
        )
        .await;
    }

    #[fbinit::test]
    async fn difference_merge_uneven(fb: FacebookInit) {
        let ctx = CoreContext::test_mock(fb);
        let repo = MergeUneven::getrepo(fb).await;
        let changeset_fetcher: ArcChangesetFetcher =
            Arc::new(TestChangesetFetcher::new(repo.clone()));
        let repo = Arc::new(repo);

        // Merge commit, and one from each branch
        let inputs = get_single_bonsai_streams(
            ctx.clone(),
            &repo,
            &[
                "d35b1875cdd1ed2c687e86f1604b9d7e989450cb",
                "4f7f3fd428bec1a48f9314414b063c706d9c1aed",
                "16839021e338500b3cf7c9b871c8a07351697d68",
            ],
        )
        .await;
        let left_nodestream =
            UnionNodeStream::new(ctx.clone(), &changeset_fetcher, inputs.into_iter()).boxify();

        // Everything from base to just before merge on one side
        let inputs = get_single_bonsai_streams(
            ctx.clone(),
            &repo,
            &[
                "16839021e338500b3cf7c9b871c8a07351697d68",
                "1d8a907f7b4bf50c6a09c16361e2205047ecc5e5",
                "3cda5c78aa35f0f5b09780d971197b51cad4613a",
                "15c40d0abc36d47fb51c8eaec51ac7aad31f669c",
            ],
        )
        .await;
        let right_nodestream =
            UnionNodeStream::new(ctx.clone(), &changeset_fetcher, inputs.into_iter()).boxify();

        let nodestream = SetDifferenceNodeStream::new(
            ctx.clone(),
            &changeset_fetcher,
            left_nodestream,
            right_nodestream,
        )
        .boxify();

        assert_changesets_sequence(
            ctx.clone(),
            &repo,
            vec![
                string_to_bonsai(fb, &repo, "d35b1875cdd1ed2c687e86f1604b9d7e989450cb").await,
                string_to_bonsai(fb, &repo, "4f7f3fd428bec1a48f9314414b063c706d9c1aed").await,
            ],
            nodestream,
        )
        .await;
    }
}
