# Copyright (c) Meta Platforms, Inc. and affiliates.
# Copyright (c) Mercurial Contributors.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2 or any later version.

from __future__ import absolute_import

from edenscm.mercurial import pycompat
from testutil.dott import feature, sh, testtmp  # noqa: F401


sh % "hg init repo"
sh % "cd repo"
sh % "touch foo"
sh % "hg add foo"
for i in range(12):
    open("foo", "ab").write(b"foo-%s\n" % pycompat.encodeutf8(str(i)))
    sh.hg("ci", "-m", "foo-%s" % i)

sh % "hg export -v -o 'foo-%nof%N.patch' 2:tip" == r"""
    exporting patches:
    foo-01of10.patch
    foo-02of10.patch
    foo-03of10.patch
    foo-04of10.patch
    foo-05of10.patch
    foo-06of10.patch
    foo-07of10.patch
    foo-08of10.patch
    foo-09of10.patch
    foo-10of10.patch"""
sh % "hg export -v -o 'foo-%%%H.patch' 2:tip" == r"""
    exporting patches:
    foo-%617188a1c80f869a7b66c85134da88a6fb145f67.patch
    foo-%dd41a5ff707a5225204105611ba49cc5c229d55f.patch
    foo-%f95a5410f8664b6e1490a4af654e4b7d41a7b321.patch
    foo-%4346bcfde53b4d9042489078bcfa9c3e28201db2.patch
    foo-%afda8c3a009cc99449a05ad8aa4655648c4ecd34.patch
    foo-%35284ce2b6b99c9d2ac66268fe99e68e1974e1aa.patch
    foo-%9688c41894e6931305fa7165a37f6568050b4e9b.patch
    foo-%747d3c68f8ec44bb35816bfcd59aeb50b9654c2f.patch
    foo-%5f17a83f5fbd9414006a5e563eab4c8a00729efd.patch
    foo-%f3acbafac161ec68f1598af38f794f28847ca5d3.patch"""
sh % "hg export -v -o 'foo-%b-%R.patch' 2:tip" == r"""
    exporting patches:
    foo-repo-2.patch
    foo-repo-3.patch
    foo-repo-4.patch
    foo-repo-5.patch
    foo-repo-6.patch
    foo-repo-7.patch
    foo-repo-8.patch
    foo-repo-9.patch
    foo-repo-10.patch
    foo-repo-11.patch"""
sh % "hg export -v -o 'foo-%h.patch' 2:tip" == r"""
    exporting patches:
    foo-617188a1c80f.patch
    foo-dd41a5ff707a.patch
    foo-f95a5410f866.patch
    foo-4346bcfde53b.patch
    foo-afda8c3a009c.patch
    foo-35284ce2b6b9.patch
    foo-9688c41894e6.patch
    foo-747d3c68f8ec.patch
    foo-5f17a83f5fbd.patch
    foo-f3acbafac161.patch"""
sh % "hg export -v -o 'foo-%r.patch' 2:tip" == r"""
    exporting patches:
    foo-02.patch
    foo-03.patch
    foo-04.patch
    foo-05.patch
    foo-06.patch
    foo-07.patch
    foo-08.patch
    foo-09.patch
    foo-10.patch
    foo-11.patch"""
sh % "hg export -v -o 'foo-%m.patch' 2:tip" == r"""
    exporting patches:
    foo-foo_2.patch
    foo-foo_3.patch
    foo-foo_4.patch
    foo-foo_5.patch
    foo-foo_6.patch
    foo-foo_7.patch
    foo-foo_8.patch
    foo-foo_9.patch
    foo-foo_10.patch
    foo-foo_11.patch"""

# Doing it again clobbers the files rather than appending:
sh % "hg export -v -o foo-%m.patch 2:3" == r"""
    exporting patches:
    foo-foo_2.patch
    foo-foo_3.patch"""
sh % "grep HG foo-foo_2.patch" | "wc -l" == "1"
sh % "grep HG foo-foo_3.patch" | "wc -l" == "1"

# Exporting 4 changesets to a file:

sh % "hg export -o export_internal 1 2 3 4"
sh % "grep HG export_internal" | "wc -l" == "4"

# Doing it again clobbers the file rather than appending:
sh % "hg export -o export_internal 1 2 3 4"
sh % "grep HG export_internal" | "wc -l" == "4"

# Exporting 4 changesets to stdout:

sh % "hg export 1 2 3 4" | "grep HG" | "wc -l" == "4"

# Exporting revision -2 to a file:

sh % "hg export -- -2" == r"""
    # HG changeset patch
    # User test
    # Date 0 0
    #      Thu Jan 01 00:00:00 1970 +0000
    # Node ID 5f17a83f5fbd9414006a5e563eab4c8a00729efd
    # Parent  747d3c68f8ec44bb35816bfcd59aeb50b9654c2f
    foo-10

    diff -r 747d3c68f8ec -r 5f17a83f5fbd foo
    --- a/foo	Thu Jan 01 00:00:00 1970 +0000
    +++ b/foo	Thu Jan 01 00:00:00 1970 +0000
    @@ -8,3 +8,4 @@
     foo-7
     foo-8
     foo-9
    +foo-10"""

# Exporting wdir revision:

sh % "echo foo-wdir" >> "foo"
sh % 'hg export "wdir()"' == r"""
    # HG changeset patch
    # User test
    # Date 0 0
    #      Thu Jan 01 00:00:00 1970 +0000
    # Node ID ffffffffffffffffffffffffffffffffffffffff
    # Parent  f3acbafac161ec68f1598af38f794f28847ca5d3


    diff -r f3acbafac161 foo
    --- a/foo	Thu Jan 01 00:00:00 1970 +0000
    +++ b/foo	Thu Jan 01 00:00:00 1970 +0000
    @@ -10,3 +10,4 @@
     foo-9
     foo-10
     foo-11
    +foo-wdir"""
sh % "hg revert -q foo"

# No filename should be printed if stdout is specified explicitly:

sh % "hg export -v 1 -o -" == r"""
    exporting patch:
    # HG changeset patch
    # User test
    # Date 0 0
    #      Thu Jan 01 00:00:00 1970 +0000
    # Node ID d1c9656e973cfb5aebd5499bbd2cb350e3b12266
    # Parent  871558de6af2e8c244222f8eea69b782c94ce3df
    foo-1

    diff -r 871558de6af2 -r d1c9656e973c foo
    --- a/foo	Thu Jan 01 00:00:00 1970 +0000
    +++ b/foo	Thu Jan 01 00:00:00 1970 +0000
    @@ -1,1 +1,2 @@
     foo-0
    +foo-1"""

# Checking if only alphanumeric characters are used in the file name (%m option):

sh % "echo line" >> "foo"
sh % 'hg commit -m " !\\"#$%&(,-./0123456789:;<=>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~"'
sh % "hg export -v -o %m.patch tip" == r"""
    exporting patch:
    ____________0123456789_______ABCDEFGHIJKLMNOPQRSTUVWXYZ______abcdefghijklmnopqrstuvwxyz____.patch"""

# Catch exporting unknown revisions (especially empty revsets, see issue3353)

sh % "hg export" == r"""
    # HG changeset patch
    # User test
    # Date 0 0
    #      Thu Jan 01 00:00:00 1970 +0000
    # Node ID 197ecd81a57f760b54f34a58817ad5b04991fa47
    # Parent  f3acbafac161ec68f1598af38f794f28847ca5d3
     !"#$%&(,-./0123456789:;<=>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\]^_`abcdefghijklmnopqrstuvwxyz{|}~

    diff -r f3acbafac161 -r 197ecd81a57f foo
    --- a/foo	Thu Jan 01 00:00:00 1970 +0000
    +++ b/foo	Thu Jan 01 00:00:00 1970 +0000
    @@ -10,3 +10,4 @@
     foo-9
     foo-10
     foo-11
    +line"""

sh % "hg export ''" == r"""
    hg: parse error: empty query
    [255]"""
sh % "hg export 999" == r"""
    abort: unknown revision '999'!
    [255]"""
sh % 'hg export "not all()"' == r"""
    abort: export requires at least one changeset
    [255]"""

# Check for color output
(
    sh % "cat"
    << r"""
[color]
mode = ansi
[extensions]
color =
"""
    >> "$HGRCPATH"
)

sh % "hg export --color always --nodates tip" == br"""
    # HG changeset patch
    # User test
    # Date 0 0
    #      Thu Jan 01 00:00:00 1970 +0000
    # Node ID * (glob)
    # Parent * (glob)
     !"#$%&(,-./0123456789:;<=>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\]^_`abcdefghijklmnopqrstuvwxyz{|}~

    \x1b[0;1mdiff -r f3acbafac161 -r 197ecd81a57f foo\x1b[0m (esc)
    \x1b[0;31;1m--- a/foo\x1b[0m (esc)
    \x1b[0;32;1m+++ b/foo\x1b[0m (esc)
    \x1b[0;35m@@ -10,3 +10,4 @@\x1b[0m (esc)
     foo-9
     foo-10
     foo-11
    \x1b[0;92m+line\x1b[0m (esc)"""

# Test exporting a subset of files

sh % "newrepo"
sh % "setconfig diff.git=1"
(
    sh % "drawdag"
    << r"""
       # B/foo/3=3\n (copied from bar/1)
       # B/foo/1=1\n (copied from bar/1)
       # B/bar/2=2\n
    B  # B/foo/2=2\n (copied from foo/1)
    |  # A/bar/1=0\n
    A  # A/foo/1=0\n
"""
)

sh % "hg export -r 'all()' --pattern 'path:foo'" == r"""
    # HG changeset patch
    # User test
    # Date 0 0
    #      Thu Jan 01 00:00:00 1970 +0000
    # Node ID * (glob)
    # Parent  0000000000000000000000000000000000000000
    A

    diff --git a/foo/1 b/foo/1
    new file mode 100644
    --- /dev/null
    +++ b/foo/1
    @@ -0,0 +1,1 @@
    +0
    # HG changeset patch
    # User test
    # Date 0 0
    #      Thu Jan 01 00:00:00 1970 +0000
    # Node ID * (glob)
    # Parent  * (glob)
    B

    diff --git a/foo/1 b/foo/1
    --- a/foo/1
    +++ b/foo/1
    @@ -1,1 +1,1 @@
    -0
    +1
    diff --git a/foo/1 b/foo/2
    copy from foo/1
    copy to foo/2
    --- a/foo/1
    +++ b/foo/2
    @@ -1,1 +1,1 @@
    -0
    +2
    diff --git a/bar/1 b/foo/3
    copy from bar/1
    copy to foo/3
    --- a/bar/1
    +++ b/foo/3
    @@ -1,1 +1,1 @@
    -0
    +3"""

# Export with diff.filtercopysource=1 - note bar/1 -> foo/3 copy is ignored

sh % "hg export -r 'all()' --pattern 'path:foo/3' --config diff.filtercopysource=0" == r"""
    # HG changeset patch
    # User test
    # Date 0 0
    #      Thu Jan 01 00:00:00 1970 +0000
    # Node ID * (glob)
    # Parent  0000000000000000000000000000000000000000
    A

    # HG changeset patch
    # User test
    # Date 0 0
    #      Thu Jan 01 00:00:00 1970 +0000
    # Node ID * (glob)
    # Parent  * (glob)
    B

    diff --git a/bar/1 b/foo/3
    copy from bar/1
    copy to foo/3
    --- a/bar/1
    +++ b/foo/3
    @@ -1,1 +1,1 @@
    -0
    +3"""

sh % "hg export -r 'all()' --pattern 'path:foo/3' --config diff.filtercopysource=1" == r"""
    # HG changeset patch
    # User test
    # Date 0 0
    #      Thu Jan 01 00:00:00 1970 +0000
    # Node ID * (glob)
    # Parent  0000000000000000000000000000000000000000
    A

    # HG changeset patch
    # User test
    # Date 0 0
    #      Thu Jan 01 00:00:00 1970 +0000
    # Node ID * (glob)
    # Parent  * (glob)
    B

    diff --git a/foo/3 b/foo/3
    new file mode 100644
    --- /dev/null
    +++ b/foo/3
    @@ -0,0 +1,1 @@
    +3"""
