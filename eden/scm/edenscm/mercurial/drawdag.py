# Portions Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# drawdag.py - convert ASCII revision DAG to actual changesets
#
# Copyright Matt Mackall <mpm@selenic.com> and others
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2 or any later version.
"""
create changesets from an ASCII graph for testing purpose.

For example, given the following input::

    c d
    |/
    b
    |
    a

4 changesets and 4 local tags will be created.
`hg log -G -T "{rev} {desc} (tag: {tags})"` will output::

    o  3 d (tag: d tip)
    |
    | o  2 c (tag: c)
    |/
    o  1 b (tag: b)
    |
    o  0 a (tag: a)

For root nodes (nodes without parents) in the graph, they can be revsets
pointing to existing nodes.  The ASCII graph could also have disconnected
components with same names referring to the same changeset.

Therefore, given the repo having the 4 changesets (and tags) above, with the
following ASCII graph as input::

    foo    bar       bar  foo
     |     /          |    |
    ancestor(c,d)     a   baz

The result (`hg log -G -T "{desc}"`) will look like::

    o    foo
    |\
    +---o  bar
    | | |
    | o |  baz
    |  /
    +---o  d
    | |
    +---o  c
    | |
    o |  b
    |/
    o  a

Some special comments could have side effects:

    - Create mutations
      # replace: A -> B -> C -> D  # chained 1 to 1 replacements
      # split: A -> B, C           # 1 to many
      # prune: A, B, C             # many to nothing
    - Create files
      # A/dir/file = line1\nline2\n
    - Remove files
      # C/A = (removed)
    - Mark as copied or renamed
      # B/B = A\n (copied from A)
      # C/C = A\n (renamed from A)
    - Specify commit dates
      # C has date 1 0
    - Disabling creating default files
      # drawdag.defaultfiles=false
    - Create bookmarks
      # bookmark BOOK_A = A
"""
from __future__ import absolute_import, print_function

import collections
import itertools
import re

import bindings

from . import (
    bookmarks,
    context,
    error,
    mutation,
    pycompat,
    scmutil,
    visibility,
)
from .i18n import _
from .node import hex, nullid, short


def _parseasciigraph(text):
    r"""str -> {str : [str]}. convert the ASCII graph to edges

    >>> import pprint
    >>> pprint.pprint({k: [vv for vv in v]
    ...  for k, v in _parseasciigraph(r'''
    ...        G
    ...        |
    ...  I D C F   # split: B -> E, F, G
    ...   \ \| |   # replace: C -> D -> H
    ...    H B E   # prune: F, I
    ...     \|/
    ...      A
    ... ''').items()})
    {'A': [],
     'B': ['A'],
     'C': ['B'],
     'D': ['B'],
     'E': ['A'],
     'F': ['E'],
     'G': ['F'],
     'H': ['A'],
     'I': ['H']}
    """
    # strip comments
    text = "\n".join(line.split("#", 1)[0] for line in text.splitlines())
    return bindings.drawdag.parse(text)


class simplefilectx(object):
    def __init__(self, path, data, renamed=None):
        assert isinstance(data, bytes)
        if b" (executable)" in data:
            data = data.replace(b" (executable)", b"")
            flags = "x"
        elif b" (symlink)" in data:
            data = data.replace(b" (symlink)", b"")
            flags = "l"
        else:
            flags = ""
        self._flags = flags
        self._data = data
        self._path = path
        self._renamed = renamed

    def data(self):
        return self._data

    def filenode(self):
        return None

    def path(self):
        return self._path

    def renamed(self):
        if self._renamed:
            return (self._renamed, nullid)
        return None

    def flags(self):
        return self._flags


class simplecommitctx(context.committablectx):
    def __init__(self, repo, name, parentctxs, filemap, mutationspec, date):
        added = []
        removed = []
        for path, data in filemap.items():
            assert isinstance(data, str)
            # check "(renamed from)". mark the source as removed
            m = re.search("\(renamed from (.+)\)\s*\Z", data, re.S)
            if m:
                removed.append(m.group(1))
            # check "(removed)"
            if re.match("\A\s*\(removed\)\s*\Z", data, re.S):
                removed.append(path)
            else:
                if path in removed:
                    raise error.Abort(_("%s: both added and removed") % path)
                added.append(path)
        extra = {"branch": "default"}
        mutinfo = None
        if mutationspec is not None:
            predctxs, cmd, split = mutationspec
            mutinfo = {
                "mutpred": ",".join(
                    [mutation.identfromnode(p.node()) for p in predctxs]
                ),
                "mutdate": date,
                "mutuser": repo.ui.config("mutation", "user") or repo.ui.username(),
                "mutop": cmd,
            }
            if split:
                mutinfo["mutsplit"] = ",".join(
                    [mutation.identfromnode(s.node()) for s in split]
                )
            if mutation.recording(repo):
                extra.update(mutinfo)
        opts = {
            "changes": scmutil.status([], added, removed, [], [], [], []),
            "date": date,
            "extra": extra,
            "mutinfo": mutinfo,
        }
        super(simplecommitctx, self).__init__(self, name, **opts)
        self._repo = repo
        self._filemap = filemap
        self._parents = parentctxs
        while len(self._parents) < 2:
            self._parents.append(repo[nullid])

    def filectx(self, key):
        data = self._filemap[key]
        m = re.match("\A(.*) \((?:renamed|copied) from (.+)\)\s*\Z", data, re.S)
        if m:
            data = m.group(1)
            renamed = m.group(2)
        else:
            renamed = None
        return simplefilectx(key, pycompat.encodeutf8(data), renamed)

    def commit(self):
        return self._repo.commitctx(self)


def _walkgraph(edges, extraedges):
    """yield node, parents in topologically order

    ``edges`` is a dict containing a mapping of each node to its parent nodes.

    ``extraedges`` is a dict containing other constraints on the ordering, e.g.
    if commit B was created by amending commit A, then this dict should have B
    -> A to ensure A is created before B.
    """
    visible = set(edges.keys())
    remaining = {}  # {str: [str]}
    for k, vs in edges.items():
        vs = vs[:]
        if k in extraedges:
            vs.extend(list(extraedges[k]))
        for v in vs:
            if v not in remaining:
                remaining[v] = []
        remaining[k] = vs
    while remaining:
        leafs = [k for k, v in remaining.items() if not v]
        if not leafs:
            raise error.Abort(_("the graph has cycles"))
        for leaf in sorted(leafs):
            if leaf in visible:
                yield leaf, edges[leaf]
            del remaining[leaf]
            for k, v in remaining.items():
                if leaf in v:
                    v.remove(leaf)


def _getcomments(text):
    """
    >>> [s for s in _getcomments(r'''
    ...        G
    ...        |
    ...  I D C F   # split: B -> E, F, G
    ...   \ \| |   # replace: C -> D -> H
    ...    H B E   # prune: F, I
    ...     \|/
    ...      A
    ... ''')]
    ['split: B -> E, F, G', 'replace: C -> D -> H', 'prune: F, I']
    """
    for line in text.splitlines():
        if " # " not in line:
            continue
        yield line.split(" # ", 1)[1].split(" # ")[0].strip()


def drawdag(repo, text, **opts):
    """given an ASCII graph as text, create changesets in repo.

    The ASCII graph is like what :hg:`log -G` outputs, with each `o` replaced
    to the name of the node. The command will create dummy changesets and local
    tags with those names to make the dummy changesets easier to be referred
    to.

    If the name of a node is a single character 'o', It will be replaced by the
    word to the right. This makes it easier to reuse
    :hg:`log -G -T '{desc}'` outputs.

    For root (no parents) nodes, revset can be used to query existing repo.
    Note that the revset cannot have confusing characters which can be seen as
    the part of the graph edges, like `|/+-\`.
    """
    with repo.wlock(), repo.lock(), repo.transaction("drawdag") as tr:
        return _drawdagintransaction(repo, text, tr, **opts)


def _drawdagintransaction(repo, text, tr, **opts):
    # parse the graph and make sure len(parents) <= 2 for each node
    edges = _parseasciigraph(text)
    for k, v in edges.items():
        if len(v) > 2:
            raise error.Abort(_("%s: too many parents: %s") % (k, " ".join(v)))

    # parse comments to get extra file content instructions
    files = collections.defaultdict(dict)  # {(name, path): content}
    comments = list(_getcomments(text))
    commenttext = "\n".join(comments)
    filere = re.compile(r"^(\w+)/([\w/]+)\s*=\s*(.*)$", re.M)
    for name, path, content in filere.findall(commenttext):
        content = content.replace(r"\n", "\n").replace(r"\1", "\1")
        files[name][path] = content

    # parse commits like "X has date 1 0" to specify dates
    dates = {}
    datere = re.compile(r"^(\w+) has date\s*[= ]([0-9 ]+)$", re.M)
    for name, date in datere.findall(commenttext):
        dates[name] = date

    # do not create default files? (ex. commit A has file "A")
    defaultfiles = not any("drawdag.defaultfiles=false" in c for c in comments)

    committed = {None: nullid}  # {name: node}
    existed = {None}

    # for leaf nodes, try to find existing nodes in repo
    for name, parents in edges.items():
        if len(parents) == 0:
            try:
                committed[name] = scmutil.revsingle(repo, name).node()
                existed.add(name)
            except error.RepoLookupError:
                pass

    # parse mutation comments like amend: A -> B -> C
    tohide = set()
    mutations = {}
    for comment in comments:
        args = comment.split(":", 1)
        if len(args) <= 1:
            continue

        cmd = args[0].strip()
        arg = args[1].strip()

        if cmd in ("replace", "rebase", "amend"):
            nodes = [n.strip() for n in arg.split("->")]
            for i in range(len(nodes) - 1):
                pred, succ = nodes[i], nodes[i + 1]
                if succ in mutations:
                    raise error.Abort(
                        _("%s: multiple mutations: from %s and %s")
                        % (succ, pred, mutations[succ][0])
                    )
                mutations[succ] = ([pred], cmd, None)
                tohide.add(pred)
        elif cmd in ("split",):
            pred, succs = arg.split("->")
            pred = pred.strip()
            succs = [s.strip() for s in succs.split(",")]
            for succ in succs:
                if succ in mutations:
                    raise error.Abort(
                        _("%s: multiple mutations: from %s and %s")
                        % (succ, pred, mutations[succ][0])
                    )
            for i in range(len(succs) - 1):
                parent = succs[i]
                child = succs[i + 1]
                if child not in edges or parent not in edges[child]:
                    raise error.Abort(
                        _("%s: split targets must be a stack: %s is not a parent of %s")
                        % (pred, parent, child)
                    )
            mutations[succs[-1]] = ([pred], cmd, succs[:-1])
            tohide.add(pred)
        elif cmd in ("fold",):
            preds, succ = arg.split("->")
            preds = [p.strip() for p in preds.split(",")]
            succ = succ.strip()
            if succ in mutations:
                raise error.Abort(
                    _("%s: multiple mutations: from %s and %s")
                    % (succ, ", ".join(preds), mutations[succ][0])
                )
            for i in range(len(preds) - 1):
                parent = preds[i]
                child = preds[i + 1]
                if child not in edges or parent not in edges[child]:
                    raise error.Abort(
                        _("%s: fold sources must be a stack: %s is not a parent of %s")
                        % (succ, parent, child)
                    )
            mutations[succ] = (preds, cmd, None)
            tohide.update(preds)
        elif cmd in ("prune",):
            for n in arg.split(","):
                n = n.strip()
                tohide.add(n)
        elif cmd in ("revive",):
            for n in arg.split(","):
                n = n.strip()
                tohide -= {n}

    # Only record mutations if mutation is enabled.
    mutationedges = {}
    mutationpreds = set()
    if mutation.enabled(repo):
        # For mutation recording to work, we must include the mutations
        # as extra edges when walking the DAG.
        for succ, (preds, cmd, split) in mutations.items():
            succs = {succ}
            mutationpreds.update(preds)
            if split:
                succs.update(split)
            for s in succs:
                mutationedges.setdefault(s, set()).update(preds)
    else:
        mutationedges = {}
        mutations = {}

    # commit in topological order
    for name, parents in _walkgraph(edges, mutationedges):
        if name in committed:
            continue
        pctxs = [repo[committed[n]] for n in parents]
        pctxs.sort(key=lambda c: c.node())
        added = {}
        if len(parents) > 1:
            # If it's a merge, take the files and contents from the parents
            for f in pctxs[1].manifest():
                if f not in pctxs[0].manifest():
                    added[f] = pycompat.decodeutf8(pctxs[1][f].data())
        else:
            # If it's not a merge, add a single file, if defaultfiles is set
            if defaultfiles:
                added[name] = name
        # add extra file contents in comments
        for path, content in files.get(name, {}).items():
            added[path] = content
        commitmutations = None
        if name in mutations:
            preds, cmd, split = mutations[name]
            if split is not None:
                split = [repo[committed[s]] for s in split]
            commitmutations = ([repo[committed[p]] for p in preds], cmd, split)

        date = dates.get(name, "0 0")
        ctx = simplecommitctx(repo, name, pctxs, added, commitmutations, date)
        n = ctx.commit()
        committed[name] = n
        if name not in mutationpreds and opts.get("bookmarks"):
            bookmarks.addbookmarks(repo, tr, [name], hex(n), True, True)

    # parse commits like "bookmark book_A=A" to specify bookmarks
    dates = {}
    bookmarkre = re.compile(r"^bookmark (\S+)\s*=\s*(\w+)$", re.M)
    for book, name in bookmarkre.findall(commenttext):
        node = committed.get(name)
        if node:
            bookmarks.addbookmarks(repo, tr, [book], hex(node), True, True)

    # update visibility (hide commits)
    hidenodes = [committed[n] for n in tohide]
    visibility.remove(repo, hidenodes)

    del committed[None]
    if opts.get("print"):
        for name, n in sorted(committed.items()):
            if name:
                repo.ui.write("%s %s\n" % (short(n), name))
    if opts.get("write_env"):
        path = opts.get("write_env")
        with open(path, "w") as f:
            for name, n in sorted(committed.items()):
                if name and name not in existed:
                    f.write("%s=%s\n" % (name, hex(n)))
