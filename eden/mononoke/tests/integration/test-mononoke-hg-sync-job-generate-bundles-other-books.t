# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

setup configuration

  $ setup_common_config blob_files
  $ cd $TESTTMP

setup repo

  $ hginit_treemanifest repo-hg
  $ cd repo-hg
  $ echo foo > a
  $ echo foo > b
  $ hg addremove && hg ci -m 'initial'
  adding a
  adding b
  $ echo 'bar' > a
  $ hg addremove && hg ci -m 'a => bar'
  $ cat >> .hg/hgrc <<EOF
  > [extensions]
  > pushrebase =
  > EOF

create master bookmark

  $ hg bookmark master_bookmark -r tip

blobimport them into Mononoke storage and start Mononoke
  $ cd ..
  $ blobimport repo-hg/.hg repo

start mononoke
  $ start_and_wait_for_mononoke_server
Make client repo
  $ hgclone_treemanifest ssh://user@dummy/repo-hg client-push --noupdate --config extensions.remotenames= -q

Push to Mononoke
  $ cd $TESTTMP/client-push
  $ cat >> .hg/hgrc <<EOF
  > [extensions]
  > pushrebase =
  > remotenames =
  > EOF
  $ hg up -q tip

  $ mkcommit pushcommit
  $ hgmn push -r . --to newbook --create -q
  $ BOOK_LOC=$(hg log -r newbook -T '{node}')

Force push
  $ hg up -q "min(all())"
  $ mkcommit forcepush
  $ hgmn push -r . --to newbook --create -q

Bookmark move
  $ hgmn push -r "$BOOK_LOC" --to newbook --pushvar NON_FAST_FORWARD=true
  pushing rev 1e43292ffbb3 to destination mononoke://$LOCALIP:$LOCAL_PORT/repo bookmark newbook
  searching for changes
  no changes found
  updating bookmark newbook

Delete a bookmark
  $ hgmn push --delete newbook
  pushing to mononoke://$LOCALIP:$LOCAL_PORT/repo
  searching for changes
  no changes found
  deleting remote bookmark newbook
  [1]

Sync it to another client
  $ cd $TESTTMP/repo-hg
  $ enable_replay_verification_hook
  $ cat >> .hg/hgrc <<EOF
  > [treemanifest]
  > treeonly=True
  > EOF
  $ cd $TESTTMP

Sync a creation of a bookmark
  $ mononoke_hg_sync repo-hg 1 2>&1 | grep 'successful sync of entries'
  * successful sync of entries [2]* (glob)

  $ cd $TESTTMP/repo-hg
  $ hg log -r newbook -T '{desc}'
  pushcommit (no-eol)
  $ cd -
  $TESTTMP

Sync force push
  $ mononoke_hg_sync repo-hg 2 2>&1 | grep 'successful sync of entries'
  * successful sync of entries [3]* (glob)

Sync bookmark move
  $ mononoke_hg_sync repo-hg 3 2>&1 | grep 'successful sync of entries'
  * successful sync of entries [4]* (glob)

Sync deletion of a bookmark
  $ mononoke_hg_sync repo-hg 4 2>&1 | grep 'successful sync of entries'
  * successful sync of entries [5]* (glob)

  $ cd $TESTTMP/repo-hg
  $ hg log -r newbook
  abort: unknown revision 'newbook'!
  [255]
