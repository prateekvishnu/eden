# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

from edenscm.mercurial import error, registrar
from edenscm.mercurial.i18n import _

from . import createremote, update, show, latest, isworkingcopy

cmdtable = {}
command = registrar.command(cmdtable)


@command("snapshot", [], "SUBCOMMAND ...")
def snapshot(ui, repo, **opts):
    """create and share snapshots with uncommitted changes"""

    raise error.Abort(
        "you need to specify a subcommand (run with --help to see a list of subcommands)"
    )


subcmd = snapshot.subcommand(
    categories=[
        ("Manage snapshots", ["create", "update"]),
        ("Query snapshots", ["show"]),
    ]
)


@subcmd(
    "createremote|create",
    [
        (
            "L",
            "lifetime",
            "",
            _(
                "how long the snapshot should last for, seconds to days supported (e.g. 60s, 90d, 1h30m)"
            ),
            _("LIFETIME"),
        ),
        (
            "",
            "max-untracked-size",
            "1000",
            _("filter out any untracked files larger than this size, in megabytes"),
            _("MAX_SIZE"),
        ),
        (
            "",
            "reuse-storage",
            None,
            _(
                "reuse same storage as latest snapshot, if possible; its lifetime won't be extended"
            ),
        ),
    ],
)
def createremotecmd(*args, **kwargs):
    """upload to the server a snapshot of the current uncommitted changes"""
    createremote.createremote(*args, **kwargs)


@subcmd(
    "update|restore|checkout|co|up",
    [
        (
            "C",
            "clean",
            None,
            _("discard uncommitted changes and untracked files (no backup)"),
        )
    ],
    _("ID"),
)
def updatecmd(*args, **kwargs):
    """download a previously created snapshot and update working copy to its state"""
    update.update(*args, **kwargs)


@subcmd(
    "show|info",
    [
        ("", "json", None, _("output in json format instead of human-readable")),
        ("", "stat", None, _("output diffstat-style summary of changes")),
    ],
    _("ID"),
)
def showcmd(*args, **kwargs):
    """gather information about the snapshot"""
    show.show(*args, **kwargs)


@subcmd(
    "isworkingcopy",
    [
        (
            "",
            "max-untracked-size",
            "",
            _("filter out any untracked files larger than this size, in megabytes"),
            _("MAX_SIZE"),
        ),
    ],
    _("ID"),
)
def isworkingcopycmd(*args, **kwargs):
    """test if a given snapshot is the working copy"""
    isworkingcopy.cmd(*args, **kwargs)


@subcmd(
    "latest",
    [
        (
            "",
            "is-working-copy",
            None,
            _("fails if there have been local changes since the latest snapshot"),
        ),
        (
            "",
            "max-untracked-size",
            "",
            _("filter out any untracked files larger than this size, in megabytes"),
            _("MAX_SIZE"),
        ),
    ],
)
def latestcmd(*args, **kwargs):
    """information regarding the latest created/restored snapshot"""
    latest.latest(*args, **kwargs)
