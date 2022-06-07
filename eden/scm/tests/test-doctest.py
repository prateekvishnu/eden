# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

from __future__ import absolute_import

import doctest
import os
import re
import sys

from hghave import require


ispy3 = sys.version_info[0] >= 3

# this is hack to make sure no escape characters are inserted into the output

if "TERM" in os.environ:
    del os.environ["TERM"]


class py3docchecker(doctest.OutputChecker):
    def check_output(self, want, got, optionflags):
        want2 = re.sub(r"""\bu(['"])(.*?)\1""", r"\1\2\1", want)  # py2: u''
        got2 = re.sub(r"""\bb(['"])(.*?)\1""", r"\1\2\1", got)  # py3: b''
        # py3: <exc.name>: b'<msg>' -> <name>: <msg>
        #      <exc.name>: <others> -> <name>: <others>
        got2 = re.sub(
            r"""^mercurial\.\w+\.(\w+): (['"])(.*?)\2""", r"\1: \3", got2, re.MULTILINE
        )
        got2 = re.sub(r"^mercurial\.\w+\.(\w+): ", r"\1: ", got2, re.MULTILINE)
        return any(
            doctest.OutputChecker.check_output(self, w, g, optionflags)
            for w, g in [(want, got), (want2, got2)]
        )


def testmod(name, optionflags=0, testtarget=None):
    __import__(name)
    mod = sys.modules[name]
    if testtarget is not None:
        mod = getattr(mod, testtarget)

    # minimal copy of doctest.testmod()
    finder = doctest.DocTestFinder()
    checker = None
    if ispy3:
        checker = py3docchecker()
    runner = doctest.DocTestRunner(checker=checker, optionflags=optionflags)
    for test in finder.find(mod, name):
        runner.run(test)
    runner.summarize()


testmod("edenscm.mercurial.changegroup")
testmod("edenscm.mercurial.changelog")
testmod("edenscm.mercurial.cmdutil")
testmod("edenscm.mercurial.color")
testmod("edenscm.mercurial.config")
testmod("edenscm.mercurial.context")
testmod("edenscm.mercurial.dagparser", optionflags=doctest.NORMALIZE_WHITESPACE)
testmod("edenscm.mercurial.dispatch")
testmod("edenscm.mercurial.drawdag")
testmod("edenscm.mercurial.encoding")
testmod("edenscm.mercurial.formatter")
testmod("edenscm.mercurial.hg")
testmod("edenscm.mercurial.hgweb.hgwebdir_mod")
testmod("edenscm.mercurial.match")
testmod("edenscm.mercurial.mdiff")
testmod("edenscm.mercurial.minirst")
testmod("edenscm.mercurial.mutation")
testmod("edenscm.mercurial.patch")
testmod("edenscm.mercurial.pathutil")
testmod("edenscm.mercurial.parser")
testmod("edenscm.mercurial.pycompat")
testmod("edenscm.mercurial.revset")
testmod("edenscm.mercurial.revsetlang")
testmod("edenscm.mercurial.scmutil")
testmod("edenscm.mercurial.smartset")
testmod("edenscm.mercurial.store")
testmod("edenscm.mercurial.templatefilters")
testmod("edenscm.mercurial.templater")
testmod("edenscm.mercurial.ui")
testmod("edenscm.mercurial.uiconfig")
testmod("edenscm.mercurial.url")
testmod("edenscm.mercurial.util")
testmod("edenscm.mercurial.util", testtarget="platform")
testmod("edenscm.hgext.commitcloud.sync")

if sys.platform == "linux":
    testmod("edenscm.testing.sh")
    testmod("edenscm.testing.t.diff")
    testmod("edenscm.testing.t.runtime")
    testmod("edenscm.testing.t.transform")
