#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

import abc
import binascii
import os
import stat as stat_mod
import struct
import typing
import unittest
from pathlib import Path
from typing import List, Optional, Sequence, Tuple

from eden.fs.cli import fsck as fsck_mod
from eden.integration.lib import edenclient, skip
from eden.integration.snapshot import snapshot as snapshot_mod, verify as verify_mod
from eden.integration.snapshot.types.basic import BasicSnapshot
from eden.test_support.temporary_directory import TemporaryDirectoryMixin


class ExpectedError(metaclass=abc.ABCMeta):
    @abc.abstractmethod
    def is_match(self, error: fsck_mod.Error) -> bool:
        pass


class MissingMaterializedInode(ExpectedError):
    def __init__(self, inode_number: int, path: str) -> None:
        super().__init__()
        self.inode_number = inode_number
        self.path = path

    def __str__(self) -> str:
        return f"MissingMaterializedInode({self.inode_number}, {self.path!r})"

    def is_match(self, error: fsck_mod.Error) -> bool:
        if not isinstance(error, fsck_mod.MissingMaterializedInode):
            return False

        if error.child.inode_number != self.inode_number:
            return False

        if error.compute_path() != self.path:
            return False

        return True


class InvalidMaterializedInode(ExpectedError):
    def __init__(
        self, inode_number: int, path: str, parent_inode: int, bad_data: bytes
    ) -> None:
        super().__init__()
        self.inode_number = inode_number
        self.path = path
        self.parent_inode_number = parent_inode
        self.bad_data = bad_data

    def __str__(self) -> str:
        return f"InvalidMaterializedInode({self.inode_number}, {self.path!r})"

    def is_match(self, error: fsck_mod.Error) -> bool:
        if not isinstance(error, fsck_mod.InvalidMaterializedInode):
            return False

        if error.inode.inode_number != self.inode_number:
            return False

        err_path = error.inode.compute_path()
        if err_path != self.path:
            return False

        return True


class OrphanFile:
    def __init__(
        self, inode_number: int, file_info: verify_mod.ExpectedFileBase
    ) -> None:
        self.inode_number = inode_number
        self.file_info = file_info

    def __str__(self) -> str:
        return f"OrphanFile({self.inode_number}, {self.file_info.path!r})"

    def __repr__(self) -> str:
        return f"OrphanDir({self.inode_number}, {self.file_info!r})"


class OrphanDir:
    def __init__(
        self, inode_number: int, path: str, contents: List[verify_mod.ExpectedFileBase]
    ) -> None:
        self.inode_number = inode_number
        self.path: Path = Path(path)
        self.contents = contents

    def __str__(self) -> str:
        return f"OrphanDir({self.inode_number}, {self.path!r})"

    def __repr__(self) -> str:
        return f"OrphanDir({self.inode_number}, {self.path!r}, {self.contents})"


class OrphanInodes(ExpectedError):
    def __init__(self, files: List[OrphanFile], dirs: List[OrphanDir]) -> None:
        super().__init__()
        self.files = files
        self.dirs = dirs

    def __str__(self) -> str:
        return f"OrphanInodes({self.files}, {self.dirs})"

    def is_match(self, error: fsck_mod.Error) -> bool:
        if not isinstance(error, fsck_mod.OrphanInodes):
            return False

        expected_orphan_files = {orphan.inode_number for orphan in self.files}
        actual_orphan_files = {
            inode_info.inode_number for inode_info in error.orphan_files
        }
        if expected_orphan_files != actual_orphan_files:
            return False

        expected_orphan_dirs = {orphan.inode_number for orphan in self.dirs}
        actual_orphan_dirs = {
            inode_info.inode_number for inode_info in error.orphan_directories
        }
        if expected_orphan_dirs != actual_orphan_dirs:
            return False

        return True


class MissingNextInodeNumber(ExpectedError):
    def __init__(self, next_inode_number: int) -> None:
        super().__init__()
        self.next_inode_number = next_inode_number

    def __str__(self) -> str:
        return f"MissingNextInodeNumber({self.next_inode_number})"

    def is_match(self, error: fsck_mod.Error) -> bool:
        if not isinstance(error, fsck_mod.MissingNextInodeNumber):
            return False

        return error.next_inode_number == self.next_inode_number


class BadNextInodeNumber(ExpectedError):
    def __init__(
        self, read_next_inode_number: int, correct_next_inode_number: int
    ) -> None:
        super().__init__()
        self.read_next_inode_number = read_next_inode_number
        self.correct_next_inode_number = correct_next_inode_number

    def __str__(self) -> str:
        return (
            "BadNextInodeNumber("
            f"{self.read_next_inode_number}, "
            f"{self.correct_next_inode_number}"
            ")"
        )

    def is_match(self, error: fsck_mod.Error) -> bool:
        if not isinstance(error, fsck_mod.BadNextInodeNumber):
            return False

        return (
            error.read_next_inode_number == self.read_next_inode_number
            and error.next_inode_number == self.correct_next_inode_number
        )


class CorruptNextInodeNumber(ExpectedError):
    def __init__(self, next_inode_number: int) -> None:
        super().__init__()
        self.next_inode_number = next_inode_number

    def __str__(self) -> str:
        return f"CorruptNextInodeNumber({self.next_inode_number})"

    def is_match(self, error: fsck_mod.Error) -> bool:
        if not isinstance(error, fsck_mod.CorruptNextInodeNumber):
            return False

        return error.next_inode_number == self.next_inode_number


@unittest.skipIf(not edenclient.can_run_eden(), "unable to run edenfs")
class SnapshotTestBase(
    unittest.TestCase, TemporaryDirectoryMixin, metaclass=abc.ABCMeta
):
    """Tests for fsck that extract the basic-20210712 snapshot, corrupt it in various
    ways, and then run fsck to try and repair it.
    """

    tmp_dir: Path
    snapshot: BasicSnapshot

    @abc.abstractmethod
    def get_snapshot_path(self) -> Path:
        raise NotImplementedError()

    def setUp(self) -> None:
        skip.skip_if_disabled(self)
        self.tmp_dir = Path(self.make_temporary_directory())
        snapshot = snapshot_mod.unpack_into(self.get_snapshot_path(), self.tmp_dir)
        self.snapshot = typing.cast(BasicSnapshot, snapshot)

    def _checkout_state_dir(self) -> Path:
        return self.snapshot.eden_state_dir / "clients" / "checkout"

    def _overlay_path(self) -> Path:
        return self._checkout_state_dir() / "local"

    def _replace_overlay_inode(self, inode_number: int, data: Optional[bytes]) -> None:
        inode_path = (
            self._overlay_path() / f"{inode_number % 256:02x}" / str(inode_number)
        )
        inode_path.unlink()
        if data is not None:
            inode_path.write_bytes(data)

    def _run_fsck(self, expected_errors: Sequence[ExpectedError]) -> Optional[Path]:
        with fsck_mod.FilesystemChecker(self._checkout_state_dir()) as fsck:
            fsck.scan_for_errors()
            self._check_expected_errors(fsck, expected_errors)
            return fsck.fix_errors()

    def _verify_contents(self, expected_files: verify_mod.ExpectedFileSet) -> None:
        verifier = verify_mod.SnapshotVerifier()
        with self.snapshot.edenfs() as eden:
            eden.start()
            verifier.verify_directory(self.snapshot.checkout_path, expected_files)

        if verifier.errors:
            self.fail(
                f"found errors when checking checkout contents: {verifier.errors}"
            )

    def _verify_orphans_extracted(
        self,
        log_dir: Path,
        expected_errors: List[ExpectedError],
        new_fsck: bool = False,
    ) -> None:
        # Build the state that we expect to find in the lost+found directory
        expected = verify_mod.ExpectedFileSet()

        for error in expected_errors:
            if isinstance(error, OrphanInodes):
                self._build_expected_orphans(expected, error, new_fsck)
            elif isinstance(error, InvalidMaterializedInode):
                # The Python fsck code extracts broken data files into a separate
                # "broken_inodes" subdirectory, but the newer C++ logic puts everything
                # into the single "lost+found" directory.
                if new_fsck:
                    extracted_path = os.path.join(
                        str(error.parent_inode_number), os.path.basename(error.path)
                    )
                    expected.add_file(extracted_path, error.bad_data, perms=0o644)

        verifier = verify_mod.SnapshotVerifier()
        verifier.verify_directory(log_dir / "lost+found", expected)
        if verifier.errors:
            self.fail(
                f"found errors when checking extracted orphan inodes: "
                f"{verifier.errors}"
            )

    def _build_expected_orphans(
        self,
        expected: verify_mod.ExpectedFileSet,
        orphan_errors: OrphanInodes,
        new_fsck: bool,
    ) -> None:
        # All of the orphan files should be extracted as regular files using their inode
        # number as the path.  We cannot tell if the inodes were originally regular
        # files, symlinks, or sockets, so everything just gets extracted as a regular
        # file.
        for orphan_file in orphan_errors.files:
            expected.add_file(
                str(orphan_file.inode_number),
                orphan_file.file_info.contents,
                perms=0o600,
            )

        # All of the orphan directories will be extracted as directories.
        # For their contents we know file types but not permissions.
        for orphan_dir in orphan_errors.dirs:
            dir_inode = Path(str(orphan_dir.inode_number))
            for expected_file in orphan_dir.contents:
                orphan_path = dir_inode / expected_file.path.relative_to(
                    orphan_dir.path
                )
                if expected_file.file_type == stat_mod.S_IFSOCK:
                    if new_fsck:
                        expected.add_file(
                            orphan_path, expected_file.contents, perms=0o600
                        )
                    else:
                        # The Python fsck code does not extract anything but regular
                        # files and symlinks, so sockets do not get extracted.
                        continue
                elif expected_file.file_type == stat_mod.S_IFLNK:
                    expected.add_symlink(
                        orphan_path, expected_file.contents, perms=0o777
                    )
                elif expected_file.file_type == stat_mod.S_IFREG:
                    expected.add_file(orphan_path, expected_file.contents, perms=0o600)
                else:
                    raise Exception("unknown file type for expected orphan inode")

    def _check_expected_errors(
        self, fsck: fsck_mod.FilesystemChecker, expected_errors: Sequence[ExpectedError]
    ) -> None:
        remaining_expected = list(expected_errors)
        unexpected_errors: List[fsck_mod.Error] = []
        for found_error in fsck.errors:
            for expected_idx, expected in enumerate(remaining_expected):
                if expected.is_match(found_error):
                    del remaining_expected[expected_idx]
                    break
            else:
                unexpected_errors.append(found_error)

        errors = []
        if unexpected_errors:
            err_list = "  \n".join(str(err) for err in unexpected_errors)
            errors.append(f"unexpected fsck errors reported:\n  {err_list}")
        if remaining_expected:
            err_list = "  \n".join(str(err) for err in remaining_expected)
            errors.append(f"did not find all expected fsck errors:\n  {err_list}")

        if errors:
            self.fail("\n".join(errors))


class Basic20210712Test(SnapshotTestBase):
    def get_snapshot_path(self) -> Path:
        return snapshot_mod.get_snapshots_root() / "basic-20210712.tar.xz"

    def _verify_fsck_and_get_log_dir(
        self,
        expected_files: verify_mod.ExpectedFileSet,
        expected_errors: List[ExpectedError],
        auto_fsck: bool,
    ) -> Path:
        if auto_fsck:
            # Remove the next-inode-number file so that edenfs will
            # automatically peform an fsck run when mounting this checkout.
            next_inode_path = self._overlay_path() / "next-inode-number"
            next_inode_path.unlink()

            # Now call _verify_contents() without ever running fsck.
            # edenfs should automatically perform the fsck steps.
            self._verify_contents(expected_files)
            log_dirs = list((self._overlay_path().parent / "fsck").iterdir())
            if len(log_dirs) != 1:
                raise Exception(
                    f"unable to find fsck log directory: candidates are {log_dirs!r}"
                )
            return log_dirs[0]

        # manual fsck
        log_dir = self._run_fsck(expected_errors)
        assert log_dir is not None
        self._run_fsck([])
        self._verify_contents(expected_files)

        return log_dir

    def test_untracked_file_removed(self) -> None:
        self._test_file_corrupted(None)

    def test_untracked_file_empty(self) -> None:
        self._test_file_corrupted(b"")

    def test_untracked_file_short_header(self) -> None:
        self._test_file_corrupted(b"OVFL\x00\x00\x00\x01")

    def _test_file_corrupted(self, data: Optional[bytes]) -> None:
        inode_number = 52  # untracked/new/normal2.txt
        path = "untracked/new/normal2.txt"
        self._replace_overlay_inode(inode_number, data)

        expected_errors: List[ExpectedError] = []
        if data is None:
            expected_errors.append(MissingMaterializedInode(inode_number, path))
        else:
            expected_errors.append(
                InvalidMaterializedInode(
                    inode_number, path, parent_inode=50, bad_data=data
                )
            )
        repaired_files = self.snapshot.get_expected_files()
        repaired_files.set_file(path, b"", perms=0o644)

        self._verify_fsck_and_get_log_dir(
            expected_files=repaired_files,
            expected_errors=expected_errors,
            auto_fsck=False,
        )

    def test_untracked_dir_removed(self) -> None:
        self._test_untracked_dir_corrupted(None, auto_fsck=False)

    def test_untracked_dir_truncated(self) -> None:
        self._test_untracked_dir_corrupted(b"", auto_fsck=False)

    def test_untracked_dir_short_header(self) -> None:
        self._test_untracked_dir_corrupted(b"OVDR\x00\x00\x00\x01", auto_fsck=False)

    def test_untracked_dir_removed_auto_fsck(self) -> None:
        self._test_untracked_dir_corrupted(None, auto_fsck=True)

    def test_untracked_dir_truncated_auto_fsck(self) -> None:
        self._test_untracked_dir_corrupted(b"", auto_fsck=True)

    def test_untracked_dir_short_header_auto_fsck(self) -> None:
        self._test_untracked_dir_corrupted(b"OVDR\x00\x00\x00\x01", auto_fsck=True)

    _short_body_data = binascii.unhexlify(
        (
            # directory header
            "4f56 4452 0000 0001 0000 0000 5bd8 fcc8"
            "0000 0000 0031 6d28 0000 0000 5bd8 fcc8"
            "0000 0000 0178 73a4 0000 0000 5bd8 fcc8"
            "0000 0000 0178 73a4 0000 0000 0000 0000"
            # partial body
            "1b04 8c0e 6576 6572 7962 6f64 792e 736f"
            "636b 15c8 8606 1648 000e 6578 6563 7574"
        ).replace(" ", "")
    )

    def test_untracked_dir_short_body(self) -> None:
        self._test_untracked_dir_corrupted(self._short_body_data, auto_fsck=False)

    def test_untracked_dir_short_body_auto_fsck(self) -> None:
        self._test_untracked_dir_corrupted(self._short_body_data, auto_fsck=True)

    def _test_untracked_dir_corrupted(
        self, data: Optional[bytes], auto_fsck: bool
    ) -> None:
        inode_number = 49  # untracked/
        self._replace_overlay_inode(inode_number, data)

        expected_errors: List[ExpectedError] = []
        if data is None:
            expected_errors.append(MissingMaterializedInode(inode_number, "untracked"))
        else:
            expected_errors.append(
                InvalidMaterializedInode(
                    inode_number, "untracked", parent_inode=1, bad_data=data
                )
            )
        repaired_files = self.snapshot.get_expected_files()
        orphan_files = [
            OrphanFile(57, repaired_files.pop("untracked/executable.exe")),
            OrphanFile(58, repaired_files.pop("untracked/everybody.sock")),
            OrphanFile(59, repaired_files.pop("untracked/owner_only.sock")),
        ]
        orphan_dirs = [
            OrphanDir(
                50,
                "untracked/new",
                [
                    repaired_files.pop("untracked/new/normal.txt"),
                    repaired_files.pop("untracked/new/normal2.txt"),
                    repaired_files.pop("untracked/new/readonly.txt"),
                    repaired_files.pop("untracked/new/subdir/abc.txt"),
                    repaired_files.pop("untracked/new/subdir/xyz.txt"),
                ],
            )
        ]
        expected_errors.append(OrphanInodes(orphan_files, orphan_dirs))

        log_dir = self._verify_fsck_and_get_log_dir(
            expected_files=repaired_files,
            expected_errors=expected_errors,
            auto_fsck=auto_fsck,
        )
        self._verify_orphans_extracted(log_dir, expected_errors, new_fsck=auto_fsck)

    def _test_truncate_main_dir(self, auto_fsck: bool) -> None:
        # inode 4 is main/
        bad_main_data = b""
        self._replace_overlay_inode(4, bad_main_data)
        expected_errors: List[ExpectedError] = [
            InvalidMaterializedInode(4, "main", parent_inode=1, bad_data=bad_main_data)
        ]

        repaired_files = self.snapshot.get_expected_files()
        orphan_files = [
            OrphanFile(60, repaired_files.pop("main/untracked.txt")),
            OrphanFile(61, repaired_files.pop("main/ignored.txt")),
        ]
        orphan_dirs = [
            OrphanDir(24, "main/loaded_dir", []),
            OrphanDir(
                25,
                "main/materialized_subdir",
                [
                    repaired_files.pop("main/materialized_subdir/script.sh"),
                    repaired_files.pop("main/materialized_subdir/test.c"),
                    repaired_files.pop("main/materialized_subdir/modified_symlink.lnk"),
                    repaired_files.pop("main/materialized_subdir/new_symlink.lnk"),
                    repaired_files.pop("main/materialized_subdir/test/socket.sock"),
                ],
            ),
            OrphanDir(
                26,
                "main/mode_changes",
                [
                    repaired_files.pop("main/mode_changes/exe_to_normal.txt"),
                    repaired_files.pop("main/mode_changes/normal_to_exe.txt"),
                    repaired_files.pop("main/mode_changes/normal_to_readonly.txt"),
                ],
            ),
            OrphanDir(
                62,
                "main/untracked_dir",
                [repaired_files.pop("main/untracked_dir/foo.txt")],
            ),
        ]
        expected_errors.append(OrphanInodes(orphan_files, orphan_dirs))

        # The following files are inside the corrupt directory, but they were never
        # materialized and so their contents will not be extracted into lost+found.
        del repaired_files["main/loaded_dir/loaded_file.c"]
        del repaired_files["main/loaded_dir/not_loaded_exe.sh"]
        del repaired_files["main/loaded_dir/not_loaded_file.c"]
        del repaired_files["main/loaded_dir/not_loaded_subdir/a.txt"]
        del repaired_files["main/loaded_dir/not_loaded_subdir/b.exe"]
        del repaired_files["main/loaded_dir/loaded_subdir/dir1/file1.txt"]
        del repaired_files["main/loaded_dir/loaded_subdir/dir2/file2.txt"]
        del repaired_files["main/materialized_subdir/unmodified.txt"]

        log_dir = self._verify_fsck_and_get_log_dir(
            expected_files=repaired_files,
            expected_errors=expected_errors,
            auto_fsck=auto_fsck,
        )
        self._verify_orphans_extracted(log_dir, expected_errors, new_fsck=auto_fsck)

    def test_main_dir_truncated(self) -> None:
        self._test_truncate_main_dir(auto_fsck=False)

    def test_main_dir_truncated_auto_fsck(self) -> None:
        self._test_truncate_main_dir(auto_fsck=True)

    # The correct next inode number for this snapshot.
    _next_inode_number = 65

    def _compute_next_inode_data(self, inode_number: int) -> bytes:
        return struct.pack("<Q", inode_number)

    def test_missing_next_inode_number(self) -> None:
        self._test_bad_next_inode_number(
            None, MissingNextInodeNumber(self._next_inode_number)
        )

        # Start eden and verify the checkout looks correct.
        # This is relatively slow, compared to running fsck itself, so we only do
        # this on one of the next-inode-number test variants.
        expected_files = self.snapshot.get_expected_files()
        self._verify_contents(expected_files)

    def test_incorrect_next_inode_number(self) -> None:
        # Test replacing the next inode number file with a value too small by 0
        self._test_bad_next_inode_number(
            self._compute_next_inode_data(self._next_inode_number - 1),
            BadNextInodeNumber(self._next_inode_number - 1, self._next_inode_number),
        )

        # Test replacing the next inode number file with a much smaller value
        self._test_bad_next_inode_number(
            self._compute_next_inode_data(10),
            BadNextInodeNumber(10, self._next_inode_number),
        )

        # Replacing the next inode number file with a larger value should not
        # be reported as an error.
        next_inode_path = self._overlay_path() / "next-inode-number"
        next_inode_path.write_bytes(self._compute_next_inode_data(12345678))
        self._run_fsck([])

    def test_corrupt_next_inode_number(self) -> None:
        self._test_bad_next_inode_number(
            b"abc", CorruptNextInodeNumber(self._next_inode_number)
        )
        self._test_bad_next_inode_number(
            b"asdfasdfasdfasdfasdfasdfasdf",
            CorruptNextInodeNumber(self._next_inode_number),
        )

    def _test_bad_next_inode_number(
        self, next_inode_data: Optional[bytes], expected_error: ExpectedError
    ) -> None:
        next_inode_path = self._overlay_path() / "next-inode-number"

        if next_inode_data is None:
            next_inode_path.unlink()
        else:
            next_inode_path.write_bytes(next_inode_data)

        log_dir = self._run_fsck([expected_error])
        assert log_dir is not None

        # Verify the contents of the next-inode-number file now
        new_data = next_inode_path.read_bytes()
        expected_data = self._compute_next_inode_data(self._next_inode_number)
        self.assertEqual(new_data, expected_data)

        # Verify that there are no more errors reported
        self._run_fsck([])

    # TODO: replace untracked dir with file
    # TODO: replace untracked file with dir
