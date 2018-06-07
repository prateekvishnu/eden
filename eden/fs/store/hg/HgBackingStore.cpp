/*
 *  Copyright (c) 2016-present, Facebook, Inc.
 *  All rights reserved.
 *
 *  This source code is licensed under the BSD-style license found in the
 *  LICENSE file in the root directory of this source tree. An additional grant
 *  of patent rights can be found in the PATENTS file in the same directory.
 *
 */
#include "HgBackingStore.h"

#include <folly/ThreadLocal.h>
#include <folly/executors/CPUThreadPoolExecutor.h>
#include <folly/executors/thread_factory/NamedThreadFactory.h>
#include <folly/futures/Future.h>
#include <folly/logging/xlog.h>

#include "eden/fs/model/Blob.h"
#include "eden/fs/model/Hash.h"
#include "eden/fs/model/Tree.h"
#include "eden/fs/store/LocalStore.h"
#include "eden/fs/store/StoreResult.h"
#include "eden/fs/store/hg/HgImporter.h"
#include "eden/fs/utils/UnboundedQueueThreadPool.h"

using folly::ByteRange;
using folly::Future;
using folly::makeFuture;
using folly::StringPiece;
using std::make_unique;
using std::unique_ptr;
using KeySpace = facebook::eden::LocalStore::KeySpace;

DEFINE_int32(
    num_hg_import_threads,
    // Why 8? 1 is materially slower but 24 is no better than 4 in a simple
    // microbenchmark that touches all files.  8 is better than 4 in the case
    // that we need to fetch a bunch from the network.
    // See benchmarks in the doc linked from D5067763.
    // Note that this number would benefit from occasional revisiting.
    8,
    "the number of hg import threads per repo");

namespace facebook {
namespace eden {

namespace {
// Thread local HgImporter. This is only initialized on HgImporter threads.
static folly::ThreadLocalPtr<Importer> threadLocalImporter;

/**
 * Checks that the thread local HgImporter is present and returns it.
 */
Importer& getThreadLocalImporter() {
  if (!threadLocalImporter) {
    throw std::logic_error(
        "Attempting to get HgImporter from non-HgImporter thread");
  }
  return *threadLocalImporter;
}

/**
 * Thread factory that sets thread name and initializes a thread local
 * HgImporter.
 */
class HgImporterThreadFactory : public folly::ThreadFactory {
 public:
  HgImporterThreadFactory(AbsolutePathPiece repository, LocalStore* localStore)
      : delegate_("HgImporter"),
        repository_(repository),
        localStore_(localStore) {}

  std::thread newThread(folly::Func&& func) override {
    return delegate_.newThread([this, func = std::move(func)]() mutable {
      threadLocalImporter.reset(new HgImporter(repository_, localStore_));
      func();
    });
  }

 private:
  folly::NamedThreadFactory delegate_;
  AbsolutePath repository_;
  LocalStore* localStore_;
};

/**
 * An inline executor that, while it exists, keeps a thread-local HgImporter
 * instance.
 */
class HgImporterTestExecutor : public folly::InlineExecutor {
 public:
  explicit HgImporterTestExecutor(Importer* importer) {
    threadLocalImporter.reset(importer);
  }

  ~HgImporterTestExecutor() {
    threadLocalImporter.release();
  }
};
} // namespace

HgBackingStore::HgBackingStore(
    AbsolutePathPiece repository,
    LocalStore* localStore,
    UnboundedQueueThreadPool* serverThreadPool)
    : localStore_(localStore),
      importThreadPool_(make_unique<folly::CPUThreadPoolExecutor>(
          FLAGS_num_hg_import_threads,
          make_unique<folly::LifoSemMPMCQueue<
              folly::CPUThreadPoolExecutor::CPUTask,
              // block if full; Eden with fail a CHECK in multiple code
              // paths if the import throws exceptions.  We should remove
              // those checks and replace them with saner exception handling
              // in the long run, but for now we avoid that problem by
              // blocking here.
              folly::QueueBehaviorIfFull::BLOCK>>(
              /* max_capacity */ FLAGS_num_hg_import_threads * 128),
          std::make_shared<HgImporterThreadFactory>(repository, localStore))),
      serverThreadPool_(serverThreadPool) {}

/**
 * Create an HgBackingStore suitable for use in unit tests. It uses an inline
 * executor to process loaded objects rather than the thread pools used in
 * production Eden.
 */
HgBackingStore::HgBackingStore(Importer* importer, LocalStore* localStore)
    : localStore_{localStore},
      importThreadPool_{std::make_unique<HgImporterTestExecutor>(importer)},
      serverThreadPool_{importThreadPool_.get()} {}

HgBackingStore::~HgBackingStore() {}

Future<unique_ptr<Tree>> HgBackingStore::getTree(const Hash& id) {
  return folly::via(
             importThreadPool_.get(),
             [id] { return getThreadLocalImporter().importTree(id); })
      // Ensure that the control moves back to the main thread pool
      // to process the caller-attached .then routine.
      .via(serverThreadPool_);
}

Future<unique_ptr<Blob>> HgBackingStore::getBlob(const Hash& id) {
  return folly::via(
             importThreadPool_.get(),
             [id] {
               auto buf = getThreadLocalImporter().importFileContents(id);
               return make_unique<Blob>(id, std::move(buf));
             })
      // Ensure that the control moves back to the main thread pool
      // to process the caller-attached .then routine.
      .via(serverThreadPool_);
}

folly::Future<folly::Unit> HgBackingStore::prefetchBlobs(
    const std::vector<Hash>& ids) const {
  return folly::via(
             importThreadPool_.get(),
             [ids] {
               getThreadLocalImporter().prefetchFiles(ids);
               return folly::unit;
             })
      // Ensure that the control moves back to the main thread pool
      // to process the caller-attached .then routine.
      .via(serverThreadPool_);
}

Future<unique_ptr<Tree>> HgBackingStore::getTreeForCommit(
    const Hash& commitID) {
  return folly::via(
             importThreadPool_.get(),
             [this, commitID] { return getTreeForCommitImpl(commitID); })
      // Ensure that the control moves back to the main thread pool
      // to process the caller-attached .then routine.
      .via(serverThreadPool_);
}

folly::Future<unique_ptr<Tree>> HgBackingStore::getTreeForCommitImpl(
    const Hash& commitID) {
  return localStore_
      ->getFuture(KeySpace::HgCommitToTreeFamily, commitID.getBytes())
      .then(
          [this,
           commitID](StoreResult result) -> folly::Future<unique_ptr<Tree>> {
            if (!result.isValid()) {
              return nullptr;
            }

            auto rootTreeHash = Hash{result.bytes()};
            XLOG(DBG5) << "found existing tree " << rootTreeHash.toString()
                       << " for mercurial commit " << commitID.toString();

            return localStore_->getTree(rootTreeHash)
                .then(
                    [rootTreeHash, commitID](std::unique_ptr<Tree> tree)
                        -> folly::Future<unique_ptr<Tree>> {
                      if (tree) {
                        return std::move(tree);
                      }
                      // No corresponding tree for this commit ID! Must
                      // re-import. This could happen if RocksDB is corrupted
                      // in some way or deleting entries races with
                      // population.
                      XLOG(WARN) << "No corresponding tree " << rootTreeHash
                                 << " for commit " << commitID
                                 << "; will import again";
                      return nullptr;
                    });
          })
      .then(
          [this,
           commitID](unique_ptr<Tree> tree) -> folly::Future<unique_ptr<Tree>> {
            if (tree) {
              return std::move(tree);
            } else {
              auto rootTreeHash =
                  getThreadLocalImporter().importManifest(commitID.toString());
              XLOG(DBG1) << "imported mercurial commit " << commitID.toString()
                         << " as tree " << rootTreeHash.toString();

              localStore_->put(
                  KeySpace::HgCommitToTreeFamily,
                  commitID,
                  rootTreeHash.getBytes());
              return localStore_->getTree(rootTreeHash);
            }
          });
}
} // namespace eden
} // namespace facebook
