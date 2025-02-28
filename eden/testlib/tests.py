# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-strict

import os
from pathlib import Path

from .base import BaseTest, hgtest
from .repo import Repo
from .types import PathLike
from .workingcopy import WorkingCopy


class TestLibTests(BaseTest):
    @hgtest
    def test_repo_setup(self, repo: Repo, wc: WorkingCopy) -> None:
        self.assertTrue(os.path.exists(os.path.join(repo.root, ".hg")))
        self.assertTrue(os.path.exists(os.path.join(wc.root, ".hg")))

    @hgtest
    def test_working_copy_edits(self, repo: Repo, wc: WorkingCopy) -> None:
        def join(path: PathLike) -> Path:
            # pyre-fixme[7]: Expected `Path` but got `str`.
            # pyre-fixme[6]: For 2nd param expected `Union[PathLike[str], str]` but
            #  got `Union[Path, File, str]`.
            return os.path.join(wc.root, path)

        def exists(path: PathLike) -> bool:
            # pyre-fixme[6]: For 2nd param expected `Union[PathLike[str], str]` but
            #  got `Union[Path, File, str]`.
            return os.path.exists(os.path.join(wc.root, path))

        def read(path: PathLike) -> str:
            return open(join(path)).read()

        # Test auto-generating path and content, with hg add
        file = wc.file()
        self.assertTrue(exists(file.path))
        self.assertEqual(read(file.path), file.path)
        self.assertEqual(wc.status().added, [file.path])

        # Test remove
        file.remove()
        self.assertFalse(file.exists())
        wc.remove(file)
        self.assertTrue(wc.status().empty())

        # Test adding a file in a directory
        file = wc.file(path="subdir/file")
        self.assertTrue(exists("subdir/file"))
        file.remove()
        wc.remove(file)

        # Test manual path and content, without hg add
        file = wc.file(path="foo", content="bar", add=False)
        self.assertTrue(exists("foo"))
        self.assertEqual(read("foo"), "bar")
        self.assertEqual(wc.status().untracked, ["foo"])

        # Test wc.add()
        wc.add(file)
        self.assertEqual(wc.status().added, ["foo"])

        # Test reads
        self.assertEqual(file.content(), "bar")
        self.assertEqual(file.binary(), b"bar")

        # Test writes
        file.write("bar2")
        self.assertEqual(read(file.path), "bar2")
        file.append("3")
        self.assertEqual(read(file.path), "bar23")

    @hgtest
    def test_working_copy_commit(self, repo: Repo, wc: WorkingCopy) -> None:
        file = wc.file()
        commit = wc.commit()
        self.assertTrue(wc.status().empty())
        self.assertEqual(commit.status().added, [file.path])

        file = wc.file(add=False)
        commit = wc.commit(
            message="my message",
            author="my author",
            date="1980-1-1 UTC",
            addremove=True,
        )
        self.assertEqual(
            wc.hg.log(
                rev=commit.hash, template="{desc}\n{author}\n{date|isodate}"
            ).stdout,
            "my message\nmy author\n1980-01-01 00:00 +0000",
        )
        self.assertEqual(commit.status().added, [file.path])

    @hgtest
    def test_working_copy_bookmark(self, repo: Repo, wc: WorkingCopy) -> None:
        wc.file()
        commit = wc.commit()

        wc.hg.bookmark("foo")
        self.assertEqual(repo.bookmarks()["foo"], commit)

    @hgtest
    def test_working_copy_checkout(self, repo: Repo, wc: WorkingCopy) -> None:
        wc.file()
        commit1 = wc.commit()
        wc.file()
        commit2 = wc.commit()

        wc.checkout(commit1)
        self.assertEqual(wc.current_commit(), commit1)
        wc.checkout(commit2)
        self.assertEqual(wc.current_commit(), commit2)

    @hgtest
    def test_drawdag(self, repo: Repo, wc: WorkingCopy) -> None:
        repo.drawdag(
            """
C
|
B
|
A
"""
        )

        self.assertEqual(
            repo.hg.smartlog(template="{desc}").stdout,
            """o  C
│
o  B
│
o  A

""",
        )


if __name__ == "__main__":
    import unittest

    unittest.main()
