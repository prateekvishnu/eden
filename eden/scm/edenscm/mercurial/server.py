# Portions Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# server.py - utility and factory of server
#
# Copyright 2005-2007 Matt Mackall <mpm@selenic.com>
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2 or any later version.

from __future__ import absolute_import

import os
import tempfile

from . import chgserver, cmdutil, commandserver, error, hgweb, pycompat, util
from .i18n import _
from .pycompat import range


def runservice(
    opts,
    parentfn=None,
    initfn=None,
    runfn=None,
    logfile=None,
    runargs=None,
    appendpid=False,
):
    """Run a command as a service."""

    def writepid(pid):
        if opts["pid_file"]:
            if appendpid:
                mode = "ab"
            else:
                mode = "wb"
            fp = open(opts["pid_file"], mode)
            fp.write(b"%d\n" % pid)
            fp.close()

    if opts["daemon"] and not opts["daemon_postexec"]:
        # Signal child process startup with file removal
        lockfd, lockpath = tempfile.mkstemp(prefix="hg-service-")
        os.close(lockfd)
        portpath = opts.get("port_file")
        if portpath:
            util.tryunlink(portpath)
        try:
            if not runargs:
                runargs = util.hgcmd() + pycompat.sysargv[1:]
            runargs.append("--daemon-postexec=unlink:%s" % lockpath)
            # Don't pass --cwd to the child process, because we've already
            # changed directory.
            for i in range(1, len(runargs)):
                if runargs[i].startswith("--cwd="):
                    del runargs[i]
                    break
                elif runargs[i].startswith("--cwd"):
                    del runargs[i : i + 2]
                    break

            def condfn():
                if portpath and not os.path.exists(portpath):
                    return False
                return not os.path.exists(lockpath)

            pid = util.rundetached(runargs, condfn)
            if pid < 0:
                raise error.Abort(_("child process failed to start"))
            writepid(pid)
        finally:
            util.tryunlink(lockpath)
        if parentfn:
            return parentfn(pid)
        else:
            return

    if initfn:
        initfn()

    if not opts["daemon"]:
        writepid(util.getpid())

    if opts["daemon_postexec"]:
        try:
            os.setsid()
        except (AttributeError, OSError):
            # OSError can happen if spawn-ext already does setsid().
            pass
        for inst in opts["daemon_postexec"]:
            if inst.startswith("unlink:"):
                lockpath = inst[7:]
                os.unlink(lockpath)
            elif inst.startswith("chdir:"):
                os.chdir(inst[6:])
            elif inst != "none":
                raise error.Abort(_("invalid value for --daemon-postexec: %s") % inst)
        util.hidewindow()
        util.stdout.flush()
        util.stderr.flush()

        nullfd = os.open(os.devnull, os.O_RDWR)
        logfilefd = nullfd
        if logfile:
            logfilefd = os.open(logfile, os.O_RDWR | os.O_CREAT | os.O_APPEND, 0o666)
        os.dup2(nullfd, 0)
        os.dup2(logfilefd, 1)
        os.dup2(logfilefd, 2)
        if nullfd not in (0, 1, 2):
            os.close(nullfd)
        if logfile and logfilefd not in (0, 1, 2):
            os.close(logfilefd)

    if runfn:
        return runfn()


_cmdservicemap = {
    "chgunix2": chgserver.chgunixservice,
    "pipe": commandserver.pipeservice,
    "unix": commandserver.unixforkingservice,
}


def _createcmdservice(ui, repo, opts):
    mode = opts["cmdserver"]
    try:
        return _cmdservicemap[mode](ui, repo, opts)
    except KeyError:
        raise error.Abort(_("unknown mode %s") % mode)


def _createhgwebservice(ui, repo, opts):
    if not ui.configbool("web", "allowhgweb", False):
        raise error.Abort(
            _("hgweb is deprecated and services should stop using it"),
            hint="set `--config web.allowhgweb=True` to bypass the block temporarily, but this will be going away soon",
        )

    # this way we can check if something was given in the command-line
    if opts.get("port"):
        opts["port"] = util.getport(opts.get("port"))

    alluis = {ui}
    if repo:
        baseui = repo.baseui
        alluis.update([repo.baseui, repo.ui])
    else:
        baseui = ui
    webconf = opts.get("web_conf") or opts.get("webdir_conf")
    if webconf:
        # load server settings (e.g. web.port) to "copied" ui, which allows
        # hgwebdir to reload webconf cleanly
        servui = ui.copy()
        servui.readconfig(webconf, sections=["web"])
        alluis.add(servui)
    else:
        servui = ui

    optlist = (
        "name templates style address port prefix ipv6"
        " accesslog errorlog certificate encoding"
    )
    for o in optlist.split():
        val = opts.get(o, "")
        if val in (None, ""):  # should check against default options instead
            continue
        for u in alluis:
            u.setconfig("web", o, val, "serve")

    app = hgweb.createapp(baseui, repo, webconf)
    return hgweb.httpservice(servui, app, opts)


def createservice(ui, repo, opts):
    if opts["cmdserver"]:
        return _createcmdservice(ui, repo, opts)
    else:
        return _createhgwebservice(ui, repo, opts)
