/*
 *  Copyright (c) 2016-present, Facebook, Inc.
 *  All rights reserved.
 *
 *  This source code is licensed under the BSD-style license found in the
 *  LICENSE file in the root directory of this source tree. An additional grant
 *  of patent rights can be found in the PATENTS file in the same directory.
 *
 */
#pragma once

#include <folly/Range.h>
#include <optional>
#ifndef EDEN_WIN
#include <folly/Subprocess.h>
#else
#include <folly/portability/IOVec.h>
#include "eden/fs/win/utils/Subprocess.h" // @manual
#endif

#include "eden/fs/eden-config.h"
#include "eden/fs/store/LocalStore.h"
#include "eden/fs/tracing/EdenStats.h"
#include "eden/fs/utils/PathFuncs.h"

namespace folly {
namespace io {
class Cursor;
}
} // namespace folly

/* forward declare support classes from mercurial */
class DatapackStore;
class UnionDatapackStore;

namespace facebook {
namespace eden {

class Blob;
class Hash;
class HgManifestImporter;
class StoreResult;
class Tree;

#ifdef EDEN_WIN
typedef HANDLE edenfd_t;
const edenfd_t kInvalidFd = INVALID_HANDLE_VALUE;
#else
typedef int edenfd_t;
constexpr edenfd_t kInvalidFd = -1;
#endif

/**
 * Options for this HgImporter.
 *
 * This is parsed from the initial CMD_STARTED response from the
 * hg_import_helper process, and contains details about the configuration
 * for this mercurial repository.
 */
struct ImporterOptions {
  /**
   * The paths to the treemanifest pack directories.
   * If this vector is empty treemanifest import should not be used.
   */
  std::vector<std::string> treeManifestPackPaths;

  /**
   * The name of the repo
   */
  std::string repoName;
};

class Importer {
 public:
  virtual ~Importer() {}

  /**
   * Import the flat manifest for the specified revision.
   *
   * Returns a Hash identifying the root Tree for the imported revision.
   */
  virtual Hash importFlatManifest(folly::StringPiece revName) = 0;

  /**
   * Resolve the manifest node for the specified revision.
   *
   * This is used to locate the mercurial tree manifest data for
   * the root tree of a given commit.
   *
   * Returns a Hash identifying the manifest node for the revision.
   */
  virtual Hash resolveManifestNode(folly::StringPiece revName) = 0;

  /**
   * Import file information
   *
   * Takes a hash identifying the requested blob.  (For instance, blob hashes
   * can be found in the TreeEntry objects generated by importFlatManifest().)
   *
   * Returns an Blob containing the file contents.
   */
  virtual std::unique_ptr<Blob> importFileContents(Hash blobHash) = 0;

  virtual void prefetchFiles(
      const std::vector<std::pair<RelativePath, Hash>>& files) = 0;

  /**
   * Import tree and store it in the datapack
   */
  virtual void fetchTree(RelativePathPiece path, Hash pathManifestNode) = 0;
};

/**
 * HgImporter provides an API for extracting data out of a mercurial
 * repository.
 *
 * Mercurial itself is in python, so some of the import logic runs as python
 * code.  HgImporter hides all of the interaction with the underlying python
 * code.
 *
 * HgImporter is thread-bound; use HgImporter only on the thread it was created
 * on.  To achieve parallelism multiple HgImporter objects can be created for
 * the same repository and used simultaneously.  HgImporter is thread-bound for
 * the following reasons:
 *
 * * HgImporter does not synchronize its own members.
 * * HgImporter accesses EdenThreadStats, and EdenThreadStats is thread-bound.
 */
class HgImporter : public Importer {
 public:
  /**
   * Create a new HgImporter object that will import data from the specified
   * repository into the given LocalStore.
   *
   * The caller is responsible for ensuring that the LocalStore object remains
   * valid for the lifetime of the HgImporter object.
   */
  HgImporter(
      AbsolutePathPiece repoPath,
      LocalStore* store,
      std::shared_ptr<EdenThreadStats>,
      std::optional<AbsolutePath> importHelperScript = std::nullopt);

  virtual ~HgImporter();

#ifndef EDEN_WIN
  folly::ProcessReturnCode debugStopHelperProcess();
#endif

  Hash importFlatManifest(folly::StringPiece revName) override;
  Hash resolveManifestNode(folly::StringPiece revName) override;
  std::unique_ptr<Blob> importFileContents(Hash blobHash) override;
  void prefetchFiles(
      const std::vector<std::pair<RelativePath, Hash>>& files) override;
  void fetchTree(RelativePathPiece path, Hash pathManifestNode) override;

  const ImporterOptions& getOptions() const;

 private:
  /**
   * Chunk header flags.
   *
   * These are flag values, designed to be bitwise ORed with each other.
   */
  enum : uint32_t {
    FLAG_ERROR = 0x01,
    FLAG_MORE_CHUNKS = 0x02,
  };
  /**
   * hg_import_helper protocol version number.
   *
   * Bump this whenever you add new commands or change the command parameters
   * or response data.  This helps us identify if edenfs somehow ends up
   * using an incompatible version of the hg_import_helper script.
   *
   * This must be kept in sync with the PROTOCOL_VERSION field in
   * hg_import_helper.py
   */
  enum : uint32_t {
    PROTOCOL_VERSION = 1,
  };
  /**
   * Flags for the CMD_STARTED response
   */
  enum StartFlag : uint32_t {
    TREEMANIFEST_SUPPORTED = 0x01,
    MONONOKE_SUPPORTED = 0x02,
  };
  /**
   * Command type values.
   *
   * See hg_import_helper.py for a more complete description of the
   * request/response formats.
   */
  enum : uint32_t {
    CMD_STARTED = 0,
    CMD_RESPONSE = 1,
    CMD_MANIFEST = 2,
    CMD_OLD_CAT_FILE = 3,
    CMD_MANIFEST_NODE_FOR_COMMIT = 4,
    CMD_FETCH_TREE = 5,
    CMD_PREFETCH_FILES = 6,
    CMD_CAT_FILE = 7,
  };
  using TransactionID = uint32_t;
  struct ChunkHeader {
    TransactionID requestID;
    uint32_t command;
    uint32_t flags;
    uint32_t dataLength;
  };

  // Forbidden copy constructor and assignment operator
  HgImporter(const HgImporter&) = delete;
  HgImporter& operator=(const HgImporter&) = delete;

  void stopHelperProcess();
  /**
   * Wait for the helper process to send a CMD_STARTED response to indicate
   * that it has started successfully.  Process the response and finish
   * setting up member variables based on the data included in the response.
   */
  ImporterOptions waitForHelperStart();

  /**
   * Read a single manifest entry from a manifest response chunk,
   * and give it to the HgManifestImporter for processing.
   *
   * The cursor argument points to the start of the manifest entry in the
   * response chunk received from the helper process.  readManifestEntry() is
   * responsible for updating the cursor to point to the next manifest entry.
   */
  static void readManifestEntry(
      HgManifestImporter& importer,
      folly::io::Cursor& cursor,
      LocalStore::WriteBatch* writeBatch);
  /**
   * Read a response chunk header from the helper process
   *
   * If the header indicates an error, this will read the full error message
   * and throw a std::runtime_error.
   *
   * This will throw an HgImporterError if there is an error communicating with
   * the hg_import_helper.py subprocess (for instance, if the helper process has
   * exited, or if the response does not contain the expected transaction ID).
   */
  ChunkHeader readChunkHeader(TransactionID txnID, folly::StringPiece cmdName);

  /**
   * Read the body of an error message, and throw it as an exception.
   */
  [[noreturn]] void readErrorAndThrow(const ChunkHeader& header);

  void readFromHelper(void* buf, size_t size, folly::StringPiece context);
  void
  writeToHelper(struct iovec* iov, size_t numIov, folly::StringPiece context);
  template <size_t N>
  void writeToHelper(
      std::array<struct iovec, N>& iov,
      folly::StringPiece context) {
    writeToHelper(iov.data(), iov.size(), context);
  }

  /**
   * Send a request to the helper process, asking it to send us the manifest
   * for the specified revision.
   */
  TransactionID sendManifestRequest(folly::StringPiece revName);
  /**
   * Send a request to the helper process, asking it to send us the contents
   * of the given file at the specified file revision.
   */
  TransactionID sendFileRequest(RelativePathPiece path, Hash fileRevHash);
  /**
   * Send a request to the helper process, asking it to send us the
   * manifest node (NOT the full manifest!) for the specified revision.
   */
  TransactionID sendManifestNodeRequest(folly::StringPiece revName);
  /**
   * Send a request to the helper process asking it to prefetch data for trees
   * under the specified path, at the specified manifest node for the given
   * path.
   */
  TransactionID sendFetchTreeRequest(
      RelativePathPiece path,
      Hash pathManifestNode);

  // Note: intentional RelativePath rather than RelativePathPiece here because
  // HgProxyHash is not movable and it was less work to make a copy here than
  // to implement its move constructor :-p
  TransactionID sendPrefetchFilesRequest(
      const std::vector<std::pair<RelativePath, Hash>>& files);

#ifndef EDEN_WIN
  folly::Subprocess helper_;
#else
  facebook::eden::Subprocess helper_;
#endif
  const AbsolutePath repoPath_;
  LocalStore* const store_{nullptr};
  std::shared_ptr<EdenThreadStats> const stats_;
  ImporterOptions options_;
  uint32_t nextRequestID_{0};
  /**
   * The input and output file descriptors to the helper subprocess.
   * We don't own these FDs, and don't need to close them--they will be closed
   * automatically by the Subprocess object.
   *
   * We simply cache them as member variables to avoid having to look them up
   * via helper_.parentFd() each time we need to use them.
   */

  edenfd_t helperIn_{kInvalidFd};
  edenfd_t helperOut_{kInvalidFd};
};

class HgImporterError : public std::exception {
 public:
  template <typename... Args>
  HgImporterError(Args&&... args)
      : message_(folly::to<std::string>(std::forward<Args>(args)...)) {}

  const char* what() const noexcept override {
    return message_.c_str();
  }

 private:
  std::string message_;
};

/**
 * A helper class that manages an HgImporter and recreates it after any error
 * communicating with the underlying python hg_import_helper.py script.
 *
 * Because HgImporter is thread-bound, HgImporterManager is also thread-bound.
 */
class HgImporterManager : public Importer {
 public:
  HgImporterManager(
      AbsolutePathPiece repoPath,
      LocalStore* store,
      std::shared_ptr<EdenThreadStats>,
      std::optional<AbsolutePath> importHelperScript = std::nullopt);

  Hash importFlatManifest(folly::StringPiece revName) override;
  Hash resolveManifestNode(folly::StringPiece revName) override;

  std::unique_ptr<Blob> importFileContents(Hash blobHash) override;
  void prefetchFiles(
      const std::vector<std::pair<RelativePath, Hash>>& files) override;
  void fetchTree(RelativePathPiece path, Hash pathManifestNode) override;

 private:
  template <typename Fn>
  auto retryOnError(Fn&& fn);

  HgImporter* getImporter();
  void resetHgImporter(const std::exception& ex);

  std::unique_ptr<HgImporter> importer_;

  const AbsolutePath repoPath_;
  LocalStore* const store_{nullptr};
  std::shared_ptr<EdenThreadStats> const stats_;
  const std::optional<AbsolutePath> importHelperScript_;
};

} // namespace eden
} // namespace facebook
