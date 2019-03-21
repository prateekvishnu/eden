  $ cat >> $HGRCPATH << EOF
  > [extensions]
  > amend =
  > directaccess=
  > commitcloud =
  > infinitepush =
  > infinitepushbackup =
  > rebase =
  > remotenames =
  > [ui]
  > ssh = python "$TESTDIR/dummyssh"
  > [experimental]
  > evolution = createmarkers, allowunstable
  > EOF

  $ mkcommit() {
  >   echo "$1" > "$1"
  >   hg commit -Aqm "$1"
  > }

Make a server
  $ hg init server
  $ cd server
  $ cat >> .hg/hgrc << EOF
  > [infinitepush]
  > server = yes
  > indextype = disk
  > storetype = disk
  > reponame = testrepo
  > EOF

  $ mkcommit "base"

  $ cd ..

Make a secondary server
  $ hg clone ssh://user@dummy/server server1 -q
  $ cd server1
  $ cat >> .hg/hgrc << EOF
  > [infinitepush]
  > server = yes
  > indextype = disk
  > storetype = disk
  > reponame = testrepo
  > EOF

  $ cd ..

Make shared part of client config
  $ cat >> shared.rc << EOF
  > [commitcloud]
  > hostname = testhost
  > servicetype = local
  > servicelocation = $TESTTMP
  > user_token_path = $TESTTMP
  > tls.notoken=True
  > EOF

Make the first clone of the server
  $ hg clone ssh://user@dummy/server client1 -q
  $ cd client1
  $ cat ../shared.rc >> .hg/hgrc
  $ hg cloud join -q

  $ cd ..

Make the second clone of the server
  $ hg clone ssh://user@dummy/server client2 -q
  $ cd client2
  $ cat ../shared.rc >> .hg/hgrc
  $ hg cloud join -q

  $ cd ..

Make a commit in the first client, and sync it
  $ cd client1
  $ mkcommit "commit1"
  $ hg cloud sync -q

  $ cd ..

Sync from the second client - the commit should appear
  $ cd client2
  $ hg cloud sync -q

  $ hg up -q tip
  $ tglog
  @  1: fa5d62c46fd7 'commit1'
  |
  o  0: d20a80d4def3 'base'
  

Make a commit in the second client, and sync it
  $ mkcommit "commit2"
  $ hg cloud sync -q

  $ cd ..

Return to the first client and configure a different paths.infinitepush
See how the migration going
  $ cd client1
  $ mkcommit "commit3"

  $ hg cloud sync --config paths.infinitepush=ssh://user@dummy/server1
  #commitcloud synchronizing 'server' with 'user/test/default'
  #commitcloud commits storage have been switched
               from: ssh://user@dummy/server
               to: ssh://user@dummy/server1
  #commitcloud some heads are missing at ssh://user@dummy/server1
  pulling from ssh://user@dummy/server
  searching for changes
  adding changesets
  adding manifests
  adding file changes
  added 1 changesets with 1 changes to 2 files (+1 heads)
  new changesets 02f6fc2b7154
  (run 'hg heads' to see heads, 'hg merge' to merge)
  pushing to ssh://user@dummy/server1
  backing up stack rooted at fa5d62c46fd7
  remote: pushing 2 commits:
  remote:     fa5d62c46fd7  commit1
  remote:     02f6fc2b7154  commit2
  backing up stack rooted at fa5d62c46fd7
  remote: pushing 2 commits:
  remote:     fa5d62c46fd7  commit1
  remote:     26d5a99991bd  commit3
  #commitcloud commits synchronized
  finished in * sec (glob)

  $ cd ..

Return to the client2, old path will not work unless the new commits have not been backed up there
New path should work fine
  $ cd client2
  $ mkcommit "commit4"
  $ hg cloud sync
  #commitcloud synchronizing 'server' with 'user/test/default'
  pulling from ssh://user@dummy/server
  abort: unknown revision '26d5a99991bd2ef9c7e76874a58f8a4dca6f6710'!
  [255]

  $ hg cloud sync --config paths.infinitepush=ssh://user@dummy/server1
  #commitcloud synchronizing 'server' with 'user/test/default'
  #commitcloud commits storage have been switched
               from: ssh://user@dummy/server
               to: ssh://user@dummy/server1
  pulling from ssh://user@dummy/server1
  searching for changes
  adding changesets
  adding manifests
  adding file changes
  added 1 changesets with 1 changes to 2 files (+1 heads)
  new changesets 26d5a99991bd
  (run 'hg heads' to see heads, 'hg merge' to merge)
  backing up stack rooted at fa5d62c46fd7
  remote: pushing 3 commits:
  remote:     fa5d62c46fd7  commit1
  remote:     02f6fc2b7154  commit2
  remote:     c701070be855  commit4
  #commitcloud commits synchronized
  finished in * sec (glob)

  $ hg cloud sync # backwards migration
  #commitcloud synchronizing 'server' with 'user/test/default'
  #commitcloud commits storage have been switched
               from: ssh://user@dummy/server1
               to: ssh://user@dummy/server
  pushing to ssh://user@dummy/server
  backing up stack rooted at fa5d62c46fd7
  remote: pushing 4 commits:
  remote:     fa5d62c46fd7  commit1
  remote:     02f6fc2b7154  commit2
  remote:     c701070be855  commit4
  remote:     26d5a99991bd  commit3
  #commitcloud commits synchronized
  finished in * sec (glob)
