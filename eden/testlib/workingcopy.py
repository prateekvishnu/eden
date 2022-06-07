# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-strict
from __future__ import annotations

import os
from pathlib import Path
from typing import Any, Dict, Generator, IO, List, Optional, TYPE_CHECKING, Union

from eden.integration.lib import edenclient

from .commit import Commit
from .file import File
from .hg import hg
from .status import Status
from .types import PathLike
from .util import new_dir, new_file, override_environ, test_globals

if TYPE_CHECKING:
    from .repo import Repo


class WorkingCopy:
    repo: Repo
    root: Path
    hg: hg

    def __init__(self, repo: Repo, root: Path) -> None:
        self.repo = repo
        self.root = root
        self.hg = hg(self.root)

    def checkout(self, destination: Union[str, Commit], clean: bool = False) -> None:
        if isinstance(destination, Commit):
            destination = destination.hash
        self.hg.checkout(destination, clean=clean)

    def status(self) -> Status:
        return Status(self.hg.status(template="json").stdout)

    def commit(
        self,
        message: Optional[str] = None,
        files: Optional[List[str]] = None,
        author: Optional[str] = None,
        date: Optional[str] = None,
        addremove: bool = False,
    ) -> Commit:
        default_data = self.repo.gen.gen_commit_data()
        files = files or []
        if message is None:
            message = default_data["message"]
        if date is None:
            date = default_data["date"]
        if author is None:
            author = "Tester Author"

        options = dict(
            message=message,
            date=date,
            addremove=addremove,
            user=author,
        )
        self.hg.commit(*files, **options)
        return self.current_commit()

    def current_commit(self) -> Commit:
        return Commit(self.repo, self.hg.log(rev=".", template="{node}").stdout)

    def file(
        self,
        path: Optional[PathLike] = None,
        content: Optional[Union[bytes, str]] = None,
        add: bool = True,
    ) -> File:
        default_path = self.repo.gen.gen_file_name()
        if path is None:
            path = default_path
        if content is None:
            content = str(path)

        file = self[path]
        file.write(content)

        if add:
            self.add(path)

        return file

    def __getitem__(self, path: PathLike) -> File:
        return File(self.root, Path(str(path)))

    def add(self, path: PathLike) -> None:
        self.hg.add(str(path))

    def remove(self, path: PathLike) -> None:
        self.hg.remove(str(path), force=True)

    # pyre-ignore[3] - pyre doesn't like that this can return str and bytes
    def open(self, path: PathLike, mode: str = "r") -> IO[Any]:
        return self[path].open(mode)

    def write(self, path: PathLike, content: str) -> None:
        self[path].write(content)

    def join(self, path: PathLike) -> Path:
        # pyre-fixme[7]: Expected `Path` but got `str`.
        return os.path.join(self.root, str(path))


class EdenWorkingCopy(WorkingCopy):
    eden: edenclient.EdenFS

    def __init__(self, repo: Repo, path: Path) -> None:
        scratch_config = new_file()
        with open(scratch_config, "w+") as f:
            template_dir = str(new_dir()).replace("\\", "\\\\")
            f.write(
                f"""
template = {template_dir}
overrides = {{}}
"""
            )

        overrides = dict(test_globals.env)
        overrides.update(
            {
                "SCRATCH_CONFIG_PATH": str(scratch_config),
                "HG_REAL_BIN": str(hg.EXEC),
            }
        )
        with override_environ(overrides):
            self.eden = edenclient.EdenFS(
                base_dir=new_dir(),
                extra_args=["--eden_logview"],
                storage_engine="memory",
            )
            self.eden.start()
            self.eden.clone(str(repo.root), str(path), allow_empty=True)

        super().__init__(repo, path)

    def cleanup(self) -> None:
        self.eden.cleanup()
