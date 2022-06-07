/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/inodes/DeferredDiffEntry.h"

#include <folly/Unit.h>
#include <folly/futures/Future.h>
#include <folly/logging/xlog.h>
#include "eden/fs/inodes/EdenMount.h"
#include "eden/fs/inodes/FileInode.h"
#include "eden/fs/inodes/TreeInode.h"
#include "eden/fs/model/BlobMetadata.h"
#include "eden/fs/store/Diff.h"
#include "eden/fs/store/DiffCallback.h"
#include "eden/fs/store/DiffContext.h"
#include "eden/fs/store/ObjectStore.h"
#include "eden/fs/utils/Bug.h"

using folly::Future;
using folly::makeFuture;
using folly::Unit;
using std::make_unique;
using std::shared_ptr;
using std::unique_ptr;

namespace facebook::eden {

namespace {

class UntrackedDiffEntry : public DeferredDiffEntry {
 public:
  UntrackedDiffEntry(
      DiffContext* context,
      RelativePath path,
      InodePtr inode,
      const GitIgnoreStack* ignore,
      bool isIgnored)
      : DeferredDiffEntry{context, std::move(path)},
        ignore_{ignore},
        isIgnored_{isIgnored},
        inode_{std::move(inode)} {}

  /*
   * This is a template just to avoid ambiguity with the prior constructor,
   * since folly::Future<X> can unfortunately be implicitly constructed from X.
   */
  template <
      typename InodeFuture,
      typename X = typename std::enable_if<
          std::is_same<folly::Future<InodePtr>, InodeFuture>::value,
          void>>
  UntrackedDiffEntry(
      DiffContext* context,
      RelativePath path,
      InodeFuture&& inodeFuture,
      const GitIgnoreStack* ignore,
      bool isIgnored)
      : DeferredDiffEntry{context, std::move(path)},
        ignore_{ignore},
        isIgnored_{isIgnored},
        inodeFuture_{std::forward<InodeFuture>(inodeFuture)} {}

  folly::Future<folly::Unit> run() override {
    // If we have an inodeFuture_ to wait on, wait for it to finish,
    // then store the resulting inode_ and invoke run() again.
    if (inodeFuture_.valid()) {
      XCHECK(!inode_) << "cannot have both inode_ and inodeFuture_ set";
      return std::move(inodeFuture_).thenValue([this](InodePtr inode) {
        inode_ = std::move(inode);
        inodeFuture_ = folly::Future<InodePtr>::makeEmpty();
        return run();
      });
    }

    auto treeInode = inode_.asTreePtrOrNull();
    if (!treeInode.get()) {
      return EDEN_BUG_FUTURE(Unit)
          << "UntrackedDiffEntry should only used with tree inodes";
    }

    // Recursively diff the untracked directory.
    return treeInode->diff(context_, getPath(), nullptr, ignore_, isIgnored_);
  }

 private:
  const GitIgnoreStack* ignore_{nullptr};
  bool isIgnored_{false};
  InodePtr inode_;
  folly::Future<InodePtr> inodeFuture_ = folly::Future<InodePtr>::makeEmpty();
};

class ModifiedDiffEntry : public DeferredDiffEntry {
 public:
  ModifiedDiffEntry(
      DiffContext* context,
      RelativePath path,
      const TreeEntry& scmEntry,
      InodePtr inode,
      const GitIgnoreStack* ignore,
      bool isIgnored)
      : DeferredDiffEntry{context, std::move(path)},
        ignore_{ignore},
        isIgnored_{isIgnored},
        scmEntry_{scmEntry},
        inode_{std::move(inode)} {}

  ModifiedDiffEntry(
      DiffContext* context,
      RelativePath path,
      const TreeEntry& scmEntry,
      folly::Future<InodePtr>&& inodeFuture,
      const GitIgnoreStack* ignore,
      bool isIgnored)
      : DeferredDiffEntry{context, std::move(path)},
        ignore_{ignore},
        isIgnored_{isIgnored},
        scmEntry_{scmEntry},
        inodeFuture_{std::move(inodeFuture)} {}

  folly::Future<folly::Unit> run() override {
    // If we have an inodeFuture_, wait on it to complete.
    // TODO: Load the inode in parallel with loading the source control data
    // below.
    if (inodeFuture_.valid()) {
      XCHECK(!inode_) << "cannot have both inode_ and inodeFuture_ set";
      return std::move(inodeFuture_).thenValue([this](InodePtr inode) {
        inode_ = std::move(inode);
        inodeFuture_ = Future<InodePtr>::makeEmpty();
        return run();
      });
    }

    if (scmEntry_.isTree()) {
      return runForScmTree();
    } else {
      return runForScmBlob();
    }
  }

 private:
  folly::Future<folly::Unit> runForScmTree() {
    auto treeInode = inode_.asTreePtrOrNull();
    if (!treeInode) {
      // This is a Tree in the source control state, but a file or symlink
      // in the current filesystem state.
      // Report this file as untracked, and everything in the source control
      // tree as removed.
      if (isIgnored_) {
        if (context_->listIgnored) {
          XLOG(DBG6) << "directory --> ignored file: " << getPath();
          context_->callback->ignoredPath(getPath(), inode_->getType());
        }
      } else {
        XLOG(DBG6) << "directory --> untracked file: " << getPath();
        context_->callback->addedPath(getPath(), inode_->getType());
      }
      // Since this is a file or symlink in the current filesystem state, but a
      // Tree in the source control state, we have to record the files from the
      // Tree as removed. We can delegate this work to the source control tree
      // differ.
      context_->callback->removedPath(getPath(), scmEntry_.getDType());
      return diffRemovedTree(context_, getPath(), scmEntry_.getHash());
    }

    {
      auto contents = treeInode->getContents().wlock();
      if (!contents->isMaterialized()) {
        if (contents->treeHash.value() == scmEntry_.getHash()) {
          // It did not change since it was loaded,
          // and it matches the scmEntry we're diffing against.
          return makeFuture();
        } else {
          context_->callback->modifiedPath(getPath(), scmEntry_.getDType());
          auto contentsHash = contents->treeHash.value();
          contents.unlock();
          return diffTrees(
              context_,
              getPath(),
              scmEntry_.getHash(),
              contentsHash,
              ignore_,
              isIgnored_);
        }
      }
    }

    // Possibly modified directory.  Load the Tree in question.
    return context_->store
        ->getTree(scmEntry_.getHash(), context_->getFetchContext())
        .semi()
        .via(&folly::QueuedImmediateExecutor::instance())
        .thenValue([this, treeInode = std::move(treeInode)](
                       shared_ptr<const Tree>&& tree) {
          return treeInode->diff(
              context_, getPath(), std::move(tree), ignore_, isIgnored_);
        });
  }

  folly::Future<folly::Unit> runForScmBlob() {
    auto fileInode = inode_.asFilePtrOrNull();
    if (!fileInode) {
      // This is a file in the source control state, but a directory
      // in the current filesystem state.
      // Report this file as removed, and everything in the source control
      // tree as untracked/ignored.
      auto path = getPath();
      XLOG(DBG5) << "removed file: " << path;
      context_->callback->removedPath(path, scmEntry_.getDType());
      context_->callback->addedPath(path, inode_->getType());
      auto treeInode = inode_.asTreePtr();
      if (isIgnored_ && !context_->listIgnored) {
        return makeFuture();
      }
      return treeInode->diff(context_, getPath(), nullptr, ignore_, isIgnored_);
    }

    return fileInode
        ->isSameAs(
            scmEntry_.getHash(),
            scmEntry_.getType(),
            context_->getFetchContext())
        .thenValue([this](bool isSame) {
          if (!isSame) {
            XLOG(DBG5) << "modified file: " << getPath();
            context_->callback->modifiedPath(getPath(), inode_->getType());
          }
        })
        .semi()
        .via(&folly::QueuedImmediateExecutor::instance());
  }

  const GitIgnoreStack* ignore_{nullptr};
  bool isIgnored_{false};
  TreeEntry scmEntry_;
  folly::Future<InodePtr> inodeFuture_ = folly::Future<InodePtr>::makeEmpty();
  InodePtr inode_;
  shared_ptr<const Tree> scmTree_;
};

class ModifiedBlobDiffEntry : public DeferredDiffEntry {
 public:
  ModifiedBlobDiffEntry(
      DiffContext* context,
      RelativePath path,
      const TreeEntry& scmEntry,
      ObjectId currentBlobHash,
      dtype_t currentDType)
      : DeferredDiffEntry{context, std::move(path)},
        scmEntry_{scmEntry},
        currentBlobHash_{std::move(currentBlobHash)},
        currentDType_{currentDType} {}

  folly::Future<folly::Unit> run() override {
    auto f1 = context_->store->getBlobSha1(
        scmEntry_.getHash(), context_->getFetchContext());
    auto f2 = context_->store->getBlobSha1(
        currentBlobHash_, context_->getFetchContext());
    return collectAllSafe(f1, f2)
        .thenValue([this](const std::tuple<Hash20, Hash20>& info) {
          const auto& [info1, info2] = info;
          if (info1 != info2) {
            XLOG(DBG5) << "modified file: " << getPath();
            context_->callback->modifiedPath(getPath(), currentDType_);
          }
        })
        .semi()
        .via(&folly::QueuedImmediateExecutor::instance());
  }

 private:
  TreeEntry scmEntry_;
  ObjectId currentBlobHash_;
  dtype_t currentDType_;
};

class ModifiedScmDiffEntry : public DeferredDiffEntry {
 public:
  ModifiedScmDiffEntry(
      DiffContext* context,
      RelativePath path,
      ObjectId scmHash,
      ObjectId wdHash,
      const GitIgnoreStack* ignore,
      bool isIgnored)
      : DeferredDiffEntry{context, std::move(path)},
        ignore_{ignore},
        isIgnored_{isIgnored},
        scmHash_{scmHash},
        wdHash_{wdHash} {}

  folly::Future<folly::Unit> run() override {
    return diffTrees(
        context_, getPath(), scmHash_, wdHash_, ignore_, isIgnored_);
  }

 private:
  const GitIgnoreStack* ignore_{nullptr};
  bool isIgnored_{false};
  ObjectId scmHash_;
  ObjectId wdHash_;
};

class AddedScmDiffEntry : public DeferredDiffEntry {
 public:
  AddedScmDiffEntry(
      DiffContext* context,
      RelativePath path,
      ObjectId wdHash,
      const GitIgnoreStack* ignore,
      bool isIgnored)
      : DeferredDiffEntry{context, std::move(path)},
        ignore_{ignore},
        isIgnored_{isIgnored},
        wdHash_{wdHash} {}

  folly::Future<folly::Unit> run() override {
    return diffAddedTree(context_, getPath(), wdHash_, ignore_, isIgnored_);
  }

 private:
  const GitIgnoreStack* ignore_{nullptr};
  bool isIgnored_{false};
  ObjectId wdHash_;
};

class RemovedScmDiffEntry : public DeferredDiffEntry {
 public:
  RemovedScmDiffEntry(DiffContext* context, RelativePath path, ObjectId scmHash)
      : DeferredDiffEntry{context, std::move(path)}, scmHash_{scmHash} {}

  folly::Future<folly::Unit> run() override {
    return diffRemovedTree(context_, getPath(), scmHash_);
  }

 private:
  ObjectId scmHash_;
};

} // unnamed namespace

unique_ptr<DeferredDiffEntry> DeferredDiffEntry::createUntrackedEntry(
    DiffContext* context,
    RelativePath path,
    InodePtr inode,
    const GitIgnoreStack* ignore,
    bool isIgnored) {
  return make_unique<UntrackedDiffEntry>(
      context, std::move(path), std::move(inode), ignore, isIgnored);
}

unique_ptr<DeferredDiffEntry>
DeferredDiffEntry::createUntrackedEntryFromInodeFuture(
    DiffContext* context,
    RelativePath path,
    Future<InodePtr>&& inodeFuture,
    const GitIgnoreStack* ignore,
    bool isIgnored) {
  return make_unique<UntrackedDiffEntry>(
      context, std::move(path), std::move(inodeFuture), ignore, isIgnored);
}

unique_ptr<DeferredDiffEntry> DeferredDiffEntry::createModifiedEntry(
    DiffContext* context,
    RelativePath path,
    const TreeEntry& scmEntry,
    InodePtr inode,
    const GitIgnoreStack* ignore,
    bool isIgnored) {
  return make_unique<ModifiedDiffEntry>(
      context, std::move(path), scmEntry, std::move(inode), ignore, isIgnored);
}

unique_ptr<DeferredDiffEntry>
DeferredDiffEntry::createModifiedEntryFromInodeFuture(
    DiffContext* context,
    RelativePath path,
    const TreeEntry& scmEntry,
    folly::Future<InodePtr>&& inodeFuture,
    const GitIgnoreStack* ignore,
    bool isIgnored) {
  return make_unique<ModifiedDiffEntry>(
      context,
      std::move(path),
      scmEntry,
      std::move(inodeFuture),
      ignore,
      isIgnored);
}

unique_ptr<DeferredDiffEntry> DeferredDiffEntry::createModifiedEntry(
    DiffContext* context,
    RelativePath path,
    const TreeEntry& scmEntry,
    ObjectId currentBlobHash,
    dtype_t currentDType) {
  return make_unique<ModifiedBlobDiffEntry>(
      context,
      std::move(path),
      scmEntry,
      std::move(currentBlobHash),
      currentDType);
}

unique_ptr<DeferredDiffEntry> DeferredDiffEntry::createModifiedScmEntry(
    DiffContext* context,
    RelativePath path,
    ObjectId scmHash,
    ObjectId wdHash,
    const GitIgnoreStack* ignore,
    bool isIgnored) {
  return make_unique<ModifiedScmDiffEntry>(
      context, std::move(path), scmHash, wdHash, ignore, isIgnored);
}

unique_ptr<DeferredDiffEntry> DeferredDiffEntry::createAddedScmEntry(
    DiffContext* context,
    RelativePath path,
    ObjectId wdHash,
    const GitIgnoreStack* ignore,
    bool isIgnored) {
  return make_unique<AddedScmDiffEntry>(
      context, std::move(path), wdHash, ignore, isIgnored);
}

unique_ptr<DeferredDiffEntry> DeferredDiffEntry::createRemovedScmEntry(
    DiffContext* context,
    RelativePath path,
    ObjectId scmHash) {
  return make_unique<RemovedScmDiffEntry>(context, std::move(path), scmHash);
}

} // namespace facebook::eden
