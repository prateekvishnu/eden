#chg-compatible

test rust clone

  $ configure modern
  $ setconfig clone.use-rust=True
  $ setconfig remotefilelog.reponame=test-repo
  $ export LOG=hgcommands::commands::clone


 Prepare Source:

  $ newremoterepo repo1
  $ setconfig paths.default=test:e1
  $ drawdag << 'EOS'
  > E
  > |
  > D
  > |
  > C
  > |
  > B
  > |
  > A
  > EOS

  $ hg push -r $E --to master --create -q
  $ hg push -r $C --to stable --create -q

Test that nonsupported options fallback to python:

  $ cd $TESTTMP
  $ hg clone -U -r $D test:e1 $TESTTMP/rev-clone
  fetching lazy changelog
  populating main commit graph
  tip commit: 9bc730a19041f9ec7cb33c626e811aa233efb18c
  fetching selected remote bookmarks

  $ git init -q git-source
  $ hg clone --git "$TESTTMP/git-source" $TESTTMP/git-clone

Test rust clone
  $ hg clone -U test:e1 $TESTTMP/rust-clone --config remotenames.selectivepulldefault='master, stable'
  TRACE hgcommands::commands::clone: performing rust clone
  TRACE hgcommands::commands::clone: fetching lazy commit data and bookmarks
  $ cd $TESTTMP/rust-clone

Check metalog is written and keys are tracked correctly
  $ hg dbsh -c 'ui.write(str(ml.get("remotenames")))'
  b'9bc730a19041f9ec7cb33c626e811aa233efb18c bookmarks remote/master\n26805aba1e600a82e93661149f2313866a221a7b bookmarks remote/stable\n' (no-eol)

Check configuration
  $ hg paths
  default = test:e1
  $ hg config remotefilelog.reponame
  test-repo

Check commits
  $ hg log -r tip -T "{desc}\n"
  E
  $ hg log -T "{desc}\n"
  E
  D
  C
  B
  A

Check basic operations
  $ hg up master
  5 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ echo newfile > newfile
  $ hg commit -Aqm 'new commit'

Test cloning with default destination
  $ cd $TESTTMP
  $ hg clone -U test:e1
  TRACE hgcommands::commands::clone: performing rust clone
  TRACE hgcommands::commands::clone: fetching lazy commit data and bookmarks
  $ cd test-repo
  $ hg log -r tip -T "{desc}\n"
  E

Test cloning failures

  $ cd $TESTTMP
  $ FAILPOINTS=run::clone=return hg clone -U test:e1 $TESTTMP/failure-clone
  TRACE hgcommands::commands::clone: performing rust clone
  TRACE hgcommands::commands::clone: fetching lazy commit data and bookmarks
  abort: Injected clone failure
  [255]
  $ [ -d $TESTTMP/failure-clone ]
  [1]

Check that preexisting directory is not removed in failure case
  $ mkdir failure-clone
  $ FAILPOINTS=run::clone=return hg clone -U test:e1 $TESTTMP/failure-clone
  TRACE hgcommands::commands::clone: performing rust clone
  TRACE hgcommands::commands::clone: fetching lazy commit data and bookmarks
  abort: Injected clone failure
  [255]
  $ [ -d $TESTTMP/failure-clone ]
  $ [ -d $TESTTMP/failure-clone/.hg ]
  [1]

Check that prexisting repo is not modified
  $ mkdir $TESTTMP/failure-clone/.hg
  $ hg clone -U test:e1 $TESTTMP/failure-clone
  abort: .hg directory already exists at clone destination
  [255]
  $ [ -d $TESTTMP/failure-clone/.hg ]

Test default-destination-dir
  $ hg clone -U test:e1 --config clone.default-destination-dir="$TESTTMP/manually-set-dir"
  TRACE hgcommands::commands::clone: performing rust clone
  TRACE hgcommands::commands::clone: fetching lazy commit data and bookmarks
  $ ls $TESTTMP | grep manually-set-dir
  manually-set-dir

Test that we get an error when not specifying a destination directory and running in plain mode
  $ HGPLAIN=1 hg clone -U test:e1
  abort: DEST was not specified
  [255]
  $ HGPLAINEXCEPT=default_clone_dir hg clone -U test:e1 --config remotefilelog.reponame=test-repo-notquite
  TRACE hgcommands::commands::clone: performing rust clone
  TRACE hgcommands::commands::clone: fetching lazy commit data and bookmarks

Not an error for bookmarks to not exist
  $ hg clone -U test:e1 $TESTTMP/no-bookmarks --config remotenames.selectivepulldefault=banana
  TRACE hgcommands::commands::clone: performing rust clone
  TRACE hgcommands::commands::clone: fetching lazy commit data and bookmarks
