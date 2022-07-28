# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

"""reset the active bookmark and working copy to a desired revision"""

import glob
import os

from edenscm.mercurial import (
    bundlerepo,
    error,
    exchange,
    extensions,
    hg,
    lock as lockmod,
    merge,
    phases,
    pycompat,
    registrar,
    scmutil,
    visibility,
)
from edenscm.mercurial.i18n import _, _n
from edenscm.mercurial.node import hex


cmdtable = {}
command = registrar.command(cmdtable)
testedwith = "ships-with-fb-hgext"


@command(
    "reset",
    [
        ("C", "clean", None, _("wipe the working copy clean when resetting")),
        ("k", "keep", None, _("keeps the old changesets the bookmark pointed" " to")),
        ("r", "rev", "", _("revision to reset to")),
    ],
    _("hg reset [REV]"),
)
def reset(ui, repo, *args, **opts):
    """moves the active bookmark and working copy parent to the desired rev

    The reset command is for moving your active bookmark and working copy to a
    different location. This is useful for undoing commits, amends, etc.

    By default, the working copy content is not touched, so you will have
    pending changes after the reset. If --clean/-C is specified, the working
    copy contents will be overwritten to match the destination revision, and you
    will not have any pending changes.

    After your bookmark and working copy have been moved, the command will
    delete any changesets that belonged only to that bookmark. Use --keep/-k to
    avoid deleting any changesets.
    """
    if args and args[0] and opts.get("rev"):
        e = _("do not use both --rev and positional argument for revision")
        raise error.Abort(e)

    rev = opts.get("rev") or (args[0] if args else ".")
    oldctx = repo["."]

    wlock = None
    try:
        wlock = repo.wlock()
        bookmark = repo._activebookmark
        ctx = _revive(repo, rev)
        _moveto(repo, bookmark, ctx, clean=opts.get("clean"))
        if not opts.get("keep"):
            _deleteunreachable(repo, oldctx)
    finally:
        wlock.release()


def _revive(repo, rev):
    """Brings the given rev back into the repository. Finding it in backup
    bundles if necessary.
    """
    unfi = repo
    try:
        ctx = unfi[rev]
    except error.RepoLookupError:
        # It could either be a revset or a stripped commit.
        pass
    else:
        visibility.add(repo, [ctx.node()])

    try:
        revs = scmutil.revrange(repo, [rev])
        if len(revs) > 1:
            raise error.Abort(_("exactly one revision must be specified"))
        if len(revs) == 1:
            return repo[revs.first()]
    except error.RepoLookupError:
        revs = []

    return _pullbundle(repo, rev)


def _pullbundle(repo, rev):
    """Find the given rev in a backup bundle and pull it back into the
    repository.
    """
    other, rev = _findbundle(repo, rev)
    if not other:
        raise error.Abort(
            "could not find '%s' in the repo or the backup" " bundles" % rev
        )
    lock = repo.lock()
    try:
        oldtip = len(repo)
        exchange.pull(repo, other, heads=[rev])

        tr = repo.transaction("phase")
        nodes = (c.node() for c in repo.set("%d:", oldtip))
        phases.retractboundary(repo, tr, 1, nodes)
        tr.close()
    finally:
        lock.release()

    if rev not in repo:
        raise error.Abort("unable to get rev %s from repo" % rev)

    return repo[rev]


def _findbundle(repo, rev):
    """Returns the backup bundle that contains the given rev. If found, it
    returns the bundle peer and the full rev hash. If not found, it return None
    and the given rev value.
    """
    ui = repo.ui
    backuppath = repo.localvfs.join("strip-backup")
    backups = list(filter(os.path.isfile, glob.glob(backuppath + "/*.hg")))
    backups.sort(key=lambda x: os.path.getmtime(x), reverse=True)
    for backup in backups:
        # Much of this is copied from the hg incoming logic
        source = os.path.relpath(backup, pycompat.getcwd())
        source = ui.expandpath(source)
        source, branches = hg.parseurl(source)
        other = hg.peer(repo, {}, source)

        quiet = ui.quiet
        try:
            ui.quiet = True
            ret = bundlerepo.getremotechanges(ui, repo, other, None, None, None)
            localother, chlist, cleanupfn = ret
            for node in chlist:
                if hex(node).startswith(rev):
                    return other, node
        except error.LookupError:
            continue
        finally:
            ui.quiet = quiet

    return None, rev


def _moveto(repo, bookmark, ctx, clean=False):
    """Moves the given bookmark and the working copy to the given revision.
    By default it does not overwrite the working copy contents unless clean is
    True.

    Assumes the wlock is already taken.
    """
    # Move working copy over
    if clean:
        merge.update(
            repo,
            ctx.node(),
            False,  # not a branchmerge
            True,  # force overwriting files
            None,
        )  # not a partial update
    else:
        # Mark any files that are different between the two as normal-lookup
        # so they show up correctly in hg status afterwards.
        wctx = repo[None]
        m1 = wctx.manifest()
        m2 = ctx.manifest()
        diff = m1.diff(m2)

        changedfiles = []
        changedfiles.extend(pycompat.iterkeys(diff))

        dirstate = repo.dirstate
        dirchanges = [f for f in dirstate if dirstate[f] != "n"]
        changedfiles.extend(dirchanges)

        if changedfiles or ctx.node() != repo["."].node():
            with dirstate.parentchange():
                dirstate.rebuild(ctx.node(), m2, changedfiles)

    # Move bookmark over
    if bookmark:
        lock = tr = None
        try:
            lock = repo.lock()
            tr = repo.transaction("reset")
            changes = [(bookmark, ctx.node())]
            repo._bookmarks.applychanges(repo, tr, changes)
            tr.close()
        finally:
            lockmod.release(lock, tr)


def _deleteunreachable(repo, ctx):
    """Deletes all ancestor and descendant commits of the given revision that
    aren't reachable from another bookmark.
    """
    keepheads = "bookmark() + ."
    try:
        extensions.find("remotenames")
        keepheads += " + remotenames()"
    except KeyError:
        pass
    hidenodes = list(repo.nodes("(draft() & ::%n) - ::(%r)", ctx.node(), keepheads))
    if hidenodes:
        with repo.lock():
            scmutil.cleanupnodes(repo, hidenodes, "reset")
        repo.ui.status(
            _n("%d changeset hidden\n", "%d changesets hidden\n", len(hidenodes))
            % len(hidenodes)
        )
