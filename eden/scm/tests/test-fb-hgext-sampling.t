#chg-compatible
#require no-fsmonitor

Setup. SCM_SAMPLING_FILEPATH needs to be cleared as some environments may
have it set.

  $ unset SCM_SAMPLING_FILEPATH

This enables Rust tracing -> sampling support in tests.
  $ export EDENSCM_TRACE_LEVEL=info

  $ mkcommit() {
  >    echo "$1" > "$1"
  >    hg add "$1"
  >    echo "add $1" > msg
  >    echo "" >> msg
  >    hg ci -l msg
  > }
Init the repo
  $ hg init testrepo
  $ cd testrepo
  $ mkcommit a
Create an extension that logs every commit and also call repo.revs twice

Create an extension that logs the call to commit
  $ cat > $TESTTMP/logcommit.py << EOF
  > from edenscm.mercurial import extensions, localrepo
  > def cb(sample):
  >   return len(sample)
  > def _commit(orig, repo, *args, **kwargs):
  >     repo.ui.log("commit", "match filter", k=1, a={"hi":"ho"})
  >     repo.ui.log("foo", "does not match filter", k=1, a={"hi":"ho"})
  >     repo.ui.log("commit", "message %s", "string", k=1, a={"hi":"ho"})
  >     return orig(repo, *args, **kwargs)
  > def extsetup(ui):
  >     extensions.wrapfunction(localrepo.localrepository, 'commit', _commit)
  >     @ui.atexit
  >     def handler():
  >       ui._measuredtimes['atexit_measured'] += 7
  >       ui.warn("atexit handler executed\n")
  > EOF


Set up the extension and set a log file
We include only the 'commit' key, only the events with that key will be
logged
  $ cat >> $HGRCPATH << EOF
  > [ui]
  > logmeasuredtimes=True
  > [sampling]
  > key.commit=commit_table
  > key.measuredtimes=measuredtimes
  > [extensions]
  > sampling=
  > EOF
  $ LOGDIR=$TESTTMP/logs
  $ mkdir $LOGDIR
  $ echo "logcommit=$TESTTMP/logcommit.py" >> $HGRCPATH
  $ echo "[sampling]" >> $HGRCPATH
  $ echo "filepath = $LOGDIR/samplingpath.txt" >> $HGRCPATH

Do a couple of commits.  We expect to log two messages per call to repo.commit.
  $ mkdir a_topdir && cd a_topdir
  $ mkcommit b
  atexit handler executed
  atexit handler executed
  $ mkcommit c
  atexit handler executed
  atexit handler executed
  >>> from edenscm.mercurial import pycompat
  >>> import json
  >>> with open("$LOGDIR/samplingpath.txt") as f:
  ...     data = f.read()
  >>> for record in data.strip("\0").split("\0"):
  ...     parsedrecord = json.loads(record)
  ...     if parsedrecord['category'] == 'commit_table':
  ...         print(' '.join([parsedrecord["data"]["msg"], parsedrecord["category"]]))
  ...         assert len(parsedrecord["data"]) == 4
  ...     elif parsedrecord['category'] == 'measuredtimes':
  ...         print('atexit_measured: ', ", ".join(sorted(parsedrecord['data'])))
  atexit_measured:  atexit_measured, fswalk_time, metrics_type, stdio_blocked
  atexit_measured:  command_duration
  match filter commit_table
  message string commit_table
  atexit_measured:  atexit_measured, fswalk_time, metrics_type, stdio_blocked
  atexit_measured:  command_duration
  atexit_measured:  atexit_measured, fswalk_time, metrics_type, stdio_blocked
  atexit_measured:  command_duration
  match filter commit_table
  message string commit_table
  atexit_measured:  atexit_measured, fswalk_time, metrics_type, stdio_blocked
  atexit_measured:  command_duration

Test topdir logging:
  $ setconfig sampling.logtopdir=True
  $ setconfig sampling.key.command_info=command_info
  $ hg st > /dev/null
  atexit handler executed
  >>> from edenscm.mercurial import json
  >>> with open("$LOGDIR/samplingpath.txt") as f:
  ...     data = f.read().strip("\0").split("\0")
  >>> print([json.loads(d)["data"]["topdir"] for d in data if "topdir" in d])
  ['a_topdir']

Test env-var logging:
  $ setconfig sampling.env_vars=TEST_VAR1,TEST_VAR2
  $ setconfig sampling.key.env_vars=env_vars
  $ export TEST_VAR1=abc
  $ export TEST_VAR2=def
  $ hg st > /dev/null
  atexit handler executed
  >>> import json, pprint
  >>> with open("$LOGDIR/samplingpath.txt") as f:
  ...     data = f.read().strip("\0").split("\0")
  >>> for jsonstr in data:
  ...     entry = json.loads(jsonstr)
  ...     if entry["category"] == "env_vars":
  ...         for k in sorted(entry["data"].keys()):
  ...             print("%s: %s" % (k, entry["data"][k]))
  env_test_var1: abc
  env_test_var2: def
  metrics_type: env_vars

Test rust traces make it to sampling file as well:
  $ > $LOGDIR/samplingpath.txt
  $ setconfig sampling.key.from_rust=hello
  $ EDENSCM_TRACE_LEVEL=info hg debugshell -c "from edenscm import tracing; tracing.info('msg', target='from_rust', hi='there')"
  atexit handler executed
  >>> import json
  >>> with open("$LOGDIR/samplingpath.txt") as f:
  ...     data = f.read().strip("\0").split("\0")
  >>> for entry in data:
  ...     parsed = json.loads(entry)
  ...     if parsed['category'] == 'hello':
  ...         print(entry)
  {"category":"hello","data":{"message":"msg","hi":"there"}}

Test ui.metrics.gauge API
  $ cat > $TESTTMP/a.py << EOF
  > def reposetup(ui, repo):
  >     ui.metrics.gauge("foo_a", 1)
  >     ui.metrics.gauge("foo_b", 2)
  >     ui.metrics.gauge("foo_b", len(repo))
  >     ui.metrics.gauge("bar")
  >     ui.metrics.gauge("bar")
  > EOF
  $ SCM_SAMPLING_FILEPATH=$TESTTMP/a.txt hg log -r null -T '.\n' --config extensions.gauge=$TESTTMP/a.py --config sampling.key.metrics=aaa
  .
  atexit handler executed
  >>> import os, json
  >>> with open(os.path.join(os.environ["TESTTMP"], "a.txt"), "r") as f:
  ...     lines = f.read().split("\0")
  ...     for line in lines:
  ...         if "foo" in line:
  ...             obj = json.loads(line)
  ...             category = obj["category"]
  ...             data = obj["data"]
  ...             print("category: %s" % category)
  ...             for k, v in sorted(data.items()):
  ...                 print("  %s=%s" % (k, v))
  category: aaa
    bar=2
    foo_a=1
    foo_b=5
    metrics_type=metrics

Metrics can be printed if devel.print-metrics is set:
  $ hg log -r null -T '.\n' --config extensions.gauge=$TESTTMP/a.py --config devel.print-metrics=1 --config devel.skip-metrics=watchman
  .
  atexit handler executed
  { metrics : { bar : 2,  foo : { a : 1,  b : 5}}}

Metrics is logged to blackbox:

  $ setconfig blackbox.track=metrics
  $ hg log -r null -T '.\n' --config extensions.gauge=$TESTTMP/a.py
  .
  atexit handler executed
  $ hg blackbox --no-timestamp --no-sid --pattern '{"legacy_log":{"service":"metrics"}}'
  [legacy][metrics] {'metrics': {'watchmanfilecount': 3, 'watchmanfreshinstances': 0}} (?)
  [legacy][metrics] {'metrics': {'bar': 2, 'foo': {'a': 1, 'b': 5}}}
  [legacy][metrics] {'metrics': {'bar': 2, 'foo': {'a': 1, 'b': 5}}}
  [legacy][metrics] {'metrics': {'bar': 2, 'foo': {'a': 1, 'b': 5}}}
  atexit handler executed

Invalid format strings don't crash Mercurial

  $ SCM_SAMPLING_FILEPATH=$TESTTMP/invalid.txt hg --config sampling.key.invalid=invalid debugsh -c 'repo.ui.log("invalid", "invalid format %s %s", "single")'
  atexit handler executed
  >>> import os, json
  >>> with open(os.path.join(os.environ["TESTTMP"], "invalid.txt"), "r") as f:
  ...     lines = f.read().split("\0")
  ...     for line in lines:
  ...         if "invalid" in line:
  ...             obj = json.loads(line)
  ...             category = obj["category"]
  ...             data = obj["data"]
  ...             print("category: %s" % category)
  ...             for k, v in sorted(data.items()):
  ...                 print("  %s=%s" % (k, v))
  category: invalid
    metrics_type=invalid
    msg=invalid format %s %s single
