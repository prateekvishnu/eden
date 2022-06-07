#!/usr/bin/env python
# Portions Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# Copyright 2006, 2007 Olivia Mackall <olivia@selenic.com>
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2 or any later version.
from __future__ import absolute_import

import os
import sys


__doc__ = """Same as `echo a >> b`, but ensures a changed mtime of b.
Without this svn will not detect workspace changes."""


text = sys.argv[1]
fname = sys.argv[2]

f = open(fname, "ab")
try:
    before = os.fstat(f.fileno()).st_mtime
    f.write(text)
    f.write("\n")
finally:
    f.close()
inc = 1
now = os.stat(fname).st_mtime
while now == before:
    t = now + inc
    inc += 1
    os.utime(fname, (t, t))
    now = os.stat(fname).st_mtime
