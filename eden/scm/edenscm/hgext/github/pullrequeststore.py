# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# Entry in metalog where GitHub pull request data is stored (for now).
METALOG_KEY = "github-experimental-pr-store"

ML_VERSION_PROPERTY = "version"
ML_COMMITS_PROPERTY = "commits"

import json

from edenscm.mercurial import mutation
from edenscm.mercurial.node import hex


class PullRequest:
    __slots__ = ("owner", "name", "number")

    def __init__(self):
        # "owner" is what the GitHub API calls the "GitHub organization."
        self.owner = None
        # name of the GitHub repo within the organization.
        self.name = None
        # integer value of the pull request.
        self.number = None


class PullRequestStore:
    def __init__(self, repo) -> None:
        self._repo = repo

    def __str__(self):
        return json.dumps(self._get_pr_data(), indent=2)

    def map_commit_to_pull_request(self, node, pull_request: PullRequest):
        pr_data = self._get_pr_data()
        commits = pr_data[ML_COMMITS_PROPERTY]
        commits[hex(node)] = {
            "owner": pull_request.owner,
            "name": pull_request.name,
            "number": pull_request.number,
        }
        with self._repo.lock(), self._repo.transaction("github"):
            ml = self._repo.metalog()
            blob = encode_pr_data(pr_data)
            ml.set(METALOG_KEY, blob)

    def find_pull_request(self, node):
        commits = self._get_commits()
        for n in mutation.allpredecessors(self._repo, [node]):
            pr = commits.get(hex(n))
            if pr:
                pull_request = PullRequest()
                pull_request.owner = pr["owner"]
                pull_request.name = pr["name"]
                pull_request.number = pr["number"]
                return pull_request
        return None

    def _get_pr_data(self):
        ml = self._repo.metalog()
        blob = ml.get(METALOG_KEY)
        if blob:
            return decode_pr_data(blob)
        else:
            # Default value for METALOG_KEY.
            return {ML_VERSION_PROPERTY: 1, ML_COMMITS_PROPERTY: {}}

    def _get_commits(self):
        pr_data = self._get_pr_data()
        return pr_data[ML_COMMITS_PROPERTY]


"""eventually, we will provide a native implementation for encoding/decoding,
but for now, we will use basic JSON encoding.
"""


def encode_pr_data(pr_data: dict) -> bytes:
    return json.dumps(pr_data).encode("utf8")


def decode_pr_data(blob: bytes) -> dict:
    blob = json.loads(blob)
    assert isinstance(blob, dict)
    return blob
