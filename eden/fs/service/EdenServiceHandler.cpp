/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/service/EdenServiceHandler.h"

#include <sys/types.h>
#include <algorithm>
#include <optional>
#include <typeinfo>

#include <fb303/ServiceData.h>
#include <folly/Conv.h>
#include <folly/FileUtil.h>
#include <folly/Portability.h>
#include <folly/String.h>
#include <folly/chrono/Conv.h>
#include <folly/container/Access.h>
#include <folly/futures/Future.h>
#include <folly/logging/Logger.h>
#include <folly/logging/LoggerDB.h>
#include <folly/logging/xlog.h>
#include <folly/stop_watch.h>
#include <folly/system/Shell.h>
#include <thrift/lib/cpp/util/EnumUtils.h>

#ifndef _WIN32
#include "eden/fs/fuse/FuseChannel.h"
#include "eden/fs/inodes/InodeTable.h"
#include "eden/fs/inodes/Overlay.h"
#include "eden/fs/nfs/Nfsd3.h"
#else
#include "eden/fs/prjfs/PrjfsChannel.h" // @manual
#endif // !_WIN32

#ifdef EDEN_HAVE_USAGE_SERVICE
#include "eden/fs/service/facebook/EdenFSSmartPlatformServiceEndpoint.h" // @manual
#endif

#include "eden/common/utils/ProcessNameCache.h"
#include "eden/fs/config/CheckoutConfig.h"
#include "eden/fs/inodes/EdenMount.h"
#include "eden/fs/inodes/FileInode.h"
#include "eden/fs/inodes/GlobNode.h"
#include "eden/fs/inodes/InodeError.h"
#include "eden/fs/inodes/InodeMap.h"
#include "eden/fs/inodes/InodeOrTreeOrEntryLoader.h"
#include "eden/fs/inodes/Traverse.h"
#include "eden/fs/inodes/TreeInode.h"
#include "eden/fs/model/Blob.h"
#include "eden/fs/model/BlobMetadata.h"
#include "eden/fs/model/Hash.h"
#include "eden/fs/model/Tree.h"
#include "eden/fs/model/TreeEntry.h"
#include "eden/fs/service/EdenServer.h"
#include "eden/fs/service/ThriftGlobImpl.h"
#include "eden/fs/service/ThriftPermissionChecker.h"
#include "eden/fs/service/ThriftUtil.h"
#include "eden/fs/service/gen-cpp2/eden_constants.h"
#include "eden/fs/service/gen-cpp2/eden_types.h"
#include "eden/fs/service/gen-cpp2/streamingeden_constants.h"
#include "eden/fs/store/BackingStore.h"
#include "eden/fs/store/Diff.h"
#include "eden/fs/store/LocalStore.h"
#include "eden/fs/store/LocalStoreCachedBackingStore.h"
#include "eden/fs/store/ObjectFetchContext.h"
#include "eden/fs/store/ObjectStore.h"
#include "eden/fs/store/PathLoader.h"
#include "eden/fs/store/ScmStatusDiffCallback.h"
#include "eden/fs/store/hg/HgQueuedBackingStore.h"
#include "eden/fs/telemetry/SessionInfo.h"
#include "eden/fs/telemetry/Tracing.h"
#include "eden/fs/utils/Bug.h"
#include "eden/fs/utils/Clock.h"
#include "eden/fs/utils/EdenError.h"
#include "eden/fs/utils/FaultInjector.h"
#include "eden/fs/utils/NotImplemented.h"
#include "eden/fs/utils/ProcUtil.h"
#include "eden/fs/utils/StatTimes.h"
#include "eden/fs/utils/UnboundedQueueExecutor.h"

using folly::Future;
using folly::makeFuture;
using folly::StringPiece;
using folly::Try;
using folly::Unit;
using std::string;
using std::unique_ptr;
using std::vector;
using namespace std::literals::string_view_literals;

namespace {
using namespace facebook::eden;

/*
 * We need a version of folly::toDelim() that accepts zero, one, or many
 * arguments so it can be used with __VA_ARGS__ in the INSTRUMENT_THRIFT_CALL()
 * macro, so we create an overloaded method, toDelimWrapper(), to achieve that
 * effect.
 */
constexpr StringPiece toDelimWrapper() {
  return "";
}

std::string toDelimWrapper(StringPiece value) {
  return value.str();
}

template <class... Args>
std::string toDelimWrapper(StringPiece arg1, const Args&... rest) {
  std::string result;
  folly::toAppendDelimFit(", ", arg1, rest..., &result);
  return result;
}

std::string logHash(StringPiece thriftArg) {
  if (thriftArg.size() == Hash20::RAW_SIZE) {
    return Hash20{folly::ByteRange{thriftArg}}.toString();
  } else if (thriftArg.size() == Hash20::RAW_SIZE * 2) {
    return Hash20{thriftArg}.toString();
  } else {
    return folly::hexlify(thriftArg);
  }
}

/**
 * Convert a vector of strings from a thrift argument to a field
 * that we can log in an INSTRUMENT_THRIFT_CALL() log message.
 *
 * This truncates very log lists to only log the first few elements.
 */
std::string toLogArg(const std::vector<std::string>& args) {
  constexpr size_t limit = 5;
  if (args.size() <= limit) {
    return "[" + folly::join(", ", args) + "]";
  } else {
    return folly::to<string>(
        "[",
        folly::join(", ", args.begin(), args.begin() + limit),
        ", and ",
        args.size() - limit,
        " more]");
  }
}
} // namespace

#define TLOG(logger, level, file, line)     \
  FB_LOG_RAW(logger, level, file, line, "") \
      << "[" << folly::RequestContext::get() << "] "

namespace /* anonymous namespace for helper functions */ {

#define EDEN_MICRO reinterpret_cast<const char*>(u8"\u00B5s")

class ThriftFetchContext : public ObjectFetchContext {
 public:
  explicit ThriftFetchContext(
      std::optional<pid_t> pid,
      folly::StringPiece endpoint)
      : pid_(pid), endpoint_(endpoint) {}

  std::optional<pid_t> getClientPid() const override {
    return pid_;
  }

  Cause getCause() const override {
    return ObjectFetchContext::Cause::Thrift;
  }

  std::optional<folly::StringPiece> getCauseDetail() const override {
    return endpoint_;
  }

  const std::unordered_map<std::string, std::string>* FOLLY_NULLABLE
  getRequestInfo() const override {
    return &requestInfo_;
  }

  /**
   * Update the request info map.
   *
   * This is not thread safe and the caller should make sure that this function
   * isn't called in an unsafe manner.
   */
  void updateRequestInfo(const std::map<std::string, std::string>& another) {
    requestInfo_.insert(another.begin(), another.end());
  }

 private:
  std::optional<pid_t> pid_;
  folly::StringPiece endpoint_;
  std::unordered_map<std::string, std::string> requestInfo_;
};

class PrefetchFetchContext : public ObjectFetchContext {
 public:
  explicit PrefetchFetchContext(
      std::optional<pid_t> pid,
      folly::StringPiece endpoint)
      : pid_(pid), endpoint_(endpoint) {}

  std::optional<pid_t> getClientPid() const override {
    return pid_;
  }

  Cause getCause() const override {
    return ObjectFetchContext::Cause::Prefetch;
  }

  std::optional<folly::StringPiece> getCauseDetail() const override {
    return endpoint_;
  }

  const std::unordered_map<std::string, std::string>* FOLLY_NULLABLE
  getRequestInfo() const override {
    return nullptr;
  }

 private:
  std::optional<pid_t> pid_;
  folly::StringPiece endpoint_;
};

// Helper class to log where the request completes in Future
class ThriftLogHelper {
 public:
  ThriftLogHelper(ThriftLogHelper&&) = delete;
  ThriftLogHelper& operator=(ThriftLogHelper&&) = delete;

  template <typename... Args>
  ThriftLogHelper(
      const folly::Logger& logger,
      folly::LogLevel level,
      folly::StringPiece itcFunctionName,
      folly::StringPiece itcFileName,
      uint32_t itcLineNumber,
      std::optional<pid_t> pid)
      : itcFunctionName_(itcFunctionName),
        itcFileName_(itcFileName),
        itcLineNumber_(itcLineNumber),
        level_(level),
        itcLogger_(logger),
        fetchContext_{pid, itcFunctionName},
        prefetchFetchContext_{pid, itcFunctionName} {}

  ~ThriftLogHelper() {
    // Logging completion time for the request
    // The line number points to where the object was originally created
    TLOG(itcLogger_, level_, itcFileName_, itcLineNumber_) << fmt::format(
        "{}() took {} {}",
        itcFunctionName_,
        itcTimer_.elapsed().count(),
        EDEN_MICRO);
  }

  PrefetchFetchContext& getPrefetchFetchContext() {
    return prefetchFetchContext_;
  }

  ThriftFetchContext& getFetchContext() {
    return fetchContext_;
  }

  folly::StringPiece getFunctionName() {
    return itcFunctionName_;
  }

 private:
  folly::StringPiece itcFunctionName_;
  folly::StringPiece itcFileName_;
  uint32_t itcLineNumber_;
  folly::LogLevel level_;
  folly::Logger itcLogger_;
  folly::stop_watch<std::chrono::microseconds> itcTimer_ = {};
  ThriftFetchContext fetchContext_;
  PrefetchFetchContext prefetchFetchContext_;
};

template <typename ReturnType>
Future<ReturnType> wrapFuture(
    std::unique_ptr<ThriftLogHelper> logHelper,
    folly::Future<ReturnType>&& f) {
  return std::move(f).ensure([logHelper = std::move(logHelper)]() {});
}

template <typename ReturnType>
ImmediateFuture<ReturnType> wrapImmediateFuture(
    std::unique_ptr<ThriftLogHelper> logHelper,
    ImmediateFuture<ReturnType>&& f) {
  return std::move(f).ensure([logHelper = std::move(logHelper)]() {});
}

#undef EDEN_MICRO

RelativePath relpathFromUserPath(StringPiece userPath) {
  if (userPath.empty() || userPath == ".") {
    return RelativePath{};
  } else {
    return RelativePath{userPath};
  }
}

facebook::eden::InodePtr inodeFromUserPath(
    facebook::eden::EdenMount& mount,
    StringPiece rootRelativePath,
    ObjectFetchContext& context) {
  auto relPath = relpathFromUserPath(rootRelativePath);
  return mount.getInodeSlow(relPath, context).get();
}
} // namespace

// INSTRUMENT_THRIFT_CALL returns a unique pointer to
// ThriftLogHelper object. The returned pointer can be used to call wrapFuture()
// to attach a log message on the completion of the Future. This must be
// called in a Thrift worker thread because the calling pid of
// getAndRegisterClientPid is stored in a thread local variable.

// When not attached to Future it will log the completion of the operation and
// time taken to complete it.
#define INSTRUMENT_THRIFT_CALL(level, ...)                            \
  ([&](folly::StringPiece functionName,                               \
       folly::StringPiece fileName,                                   \
       uint32_t lineNumber) {                                         \
    static folly::Logger logger("eden.thrift." + functionName.str()); \
    TLOG(logger, folly::LogLevel::level, fileName, lineNumber)        \
        << functionName << "(" << toDelimWrapper(__VA_ARGS__) << ")"; \
    return std::make_unique<ThriftLogHelper>(                         \
        logger,                                                       \
        folly::LogLevel::level,                                       \
        functionName,                                                 \
        fileName,                                                     \
        lineNumber,                                                   \
        getAndRegisterClientPid());                                   \
  }(__func__, __FILE__, __LINE__))

// INSTRUMENT_THRIFT_CALL_WITH_FUNCTION_NAME_AND_PID works in the same way
// as INSTRUMENT_THRIFT_CALL but takes the function name and pid
// as a parameter in case of using inside of a lambda (in which case
// __func__ is "()"). Also, the pid passed to this function must be
// obtained from a Thrift worker thread because the calling pid is
// stored in a thread local variable.

#define INSTRUMENT_THRIFT_CALL_WITH_FUNCTION_NAME_AND_PID(            \
    level, functionName, pid, ...)                                    \
  ([&](folly::StringPiece fileName, uint32_t lineNumber) {            \
    static folly::Logger logger(                                      \
        "eden.thrift." + folly::to<string>(functionName));            \
    TLOG(logger, folly::LogLevel::level, fileName, lineNumber)        \
        << functionName << "(" << toDelimWrapper(__VA_ARGS__) << ")"; \
    return std::make_unique<ThriftLogHelper>(                         \
        logger,                                                       \
        folly::LogLevel::level,                                       \
        functionName,                                                 \
        fileName,                                                     \
        lineNumber,                                                   \
        pid);                                                         \
  }(__FILE__, __LINE__))

namespace facebook::eden {

const char* const kServiceName = "EdenFS";

EdenServiceHandler::EdenServiceHandler(
    std::vector<std::string> originalCommandLine,
    EdenServer* server)
    : BaseService{kServiceName},
      originalCommandLine_{std::move(originalCommandLine)},
      server_{server} {
  struct HistConfig {
    int64_t bucketSize{250};
    int64_t min{0};
    int64_t max{25000};
  };

  static constexpr std::pair<StringPiece, HistConfig> customMethodConfigs[] = {
      {"listMounts", {20, 0, 1000}},
      {"resetParentCommits", {20, 0, 1000}},
      {"getCurrentJournalPosition", {20, 0, 1000}},
      {"flushStatsNow", {20, 0, 1000}},
      {"reloadConfig", {200, 0, 10000}},
  };

  apache::thrift::metadata::ThriftServiceMetadataResponse metadataResponse;
  getProcessor()->getServiceMetadata(metadataResponse);
  auto& edenService =
      metadataResponse.metadata_ref()->services_ref()->at("eden.EdenService");
  for (auto& function : *edenService.functions_ref()) {
    HistConfig hc;
    for (auto& [name, customHistConfig] : customMethodConfigs) {
      if (*function.name_ref() == name) {
        hc = customHistConfig;
        break;
      }
    }
    // For now, only register EdenService methods, but we could traverse up
    // parent services too.
    static constexpr StringPiece prefix = "EdenService.";
    exportThriftFuncHist(
        folly::to<std::string>(prefix, *function.name_ref()),
        facebook::fb303::PROCESS,
        folly::small_vector<int>({50, 90, 99}), // percentiles to record
        hc.bucketSize,
        hc.min,
        hc.max);
  }
#ifdef EDEN_HAVE_USAGE_SERVICE
  spServiceEndpoint_ = std::make_unique<EdenFSSmartPlatformServiceEndpoint>(
      server_->getServerState()->getThreadPool(),
      server_->getServerState()->getEdenConfig());
#endif
}

EdenServiceHandler::~EdenServiceHandler() = default;

std::unique_ptr<apache::thrift::AsyncProcessor>
EdenServiceHandler::getProcessor() {
  auto processor = StreamingEdenServiceSvIf::getProcessor();
  if (server_->getServerState()
          ->getEdenConfig()
          ->thriftUseCustomPermissionChecking.getValue()) {
    processor->addEventHandler(
        std::make_shared<ThriftPermissionChecker>(server_->getServerState()));
  }
  return processor;
}

void EdenServiceHandler::mount(std::unique_ptr<MountArgument> argument) {
  auto helper = INSTRUMENT_THRIFT_CALL(INFO, argument->get_mountPoint());
  try {
    auto initialConfig = CheckoutConfig::loadFromClientDirectory(
        AbsolutePathPiece{*argument->mountPoint_ref()},
        AbsolutePathPiece{*argument->edenClientPath_ref()});

    server_->mount(std::move(initialConfig), *argument->readOnly_ref()).get();
  } catch (const EdenError& ex) {
    XLOG(ERR) << "Error: " << ex.what();
    throw;
  } catch (const std::exception& ex) {
    XLOG(ERR) << "Error: " << ex.what();
    throw newEdenError(ex);
  }
}

void EdenServiceHandler::unmount(std::unique_ptr<std::string> mountPoint) {
  auto helper = INSTRUMENT_THRIFT_CALL(INFO, *mountPoint);
  try {
    auto mountPath = AbsolutePathPiece{*mountPoint};
    server_->unmount(mountPath).get();
  } catch (const EdenError&) {
    throw;
  } catch (const std::exception& ex) {
    throw newEdenError(ex);
  }
}

void EdenServiceHandler::listMounts(std::vector<MountInfo>& results) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3);
  for (const auto& edenMount : server_->getAllMountPoints()) {
    MountInfo info;
    info.mountPoint_ref() = edenMount->getPath().value();
    info.edenClientPath_ref() =
        edenMount->getCheckoutConfig()->getClientDirectory().value();
    info.state_ref() = edenMount->getState();
    info.backingRepoPath_ref() =
        edenMount->getCheckoutConfig()->getRepoSource();
    results.push_back(info);
  }
}

void EdenServiceHandler::checkOutRevision(
    std::vector<CheckoutConflict>& results,
    std::unique_ptr<std::string> mountPoint,
    std::unique_ptr<std::string> hash,
    CheckoutMode checkoutMode,
    std::unique_ptr<CheckOutRevisionParams> params) {
  auto helper = INSTRUMENT_THRIFT_CALL(
      DBG1,
      *mountPoint,
      logHash(*hash),
      apache::thrift::util::enumName(checkoutMode, "(unknown)"),
      params->hgRootManifest_ref().has_value()
          ? logHash(*params->hgRootManifest_ref())
          : "(unspecified hg root manifest)");

  auto mountPath = AbsolutePathPiece{*mountPoint};
  auto checkoutFuture = server_->checkOutRevision(
      mountPath,
      *hash,
      params->hgRootManifest_ref().to_optional(),
      helper->getFetchContext().getClientPid(),
      helper->getFunctionName(),
      checkoutMode);
  results = std::move(std::move(checkoutFuture).get().conflicts);
}

void EdenServiceHandler::resetParentCommits(
    std::unique_ptr<std::string> mountPoint,
    std::unique_ptr<WorkingDirectoryParents> parents,
    std::unique_ptr<ResetParentCommitsParams> params) {
  auto helper = INSTRUMENT_THRIFT_CALL(
      DBG1,
      *mountPoint,
      logHash(*parents->parent1_ref()),
      params->hgRootManifest_ref().has_value()
          ? logHash(*params->hgRootManifest_ref())
          : "(unspecified hg root manifest)");

  auto mountPath = AbsolutePathPiece{*mountPoint};
  auto edenMount = server_->getMount(mountPath);
  auto parent1 =
      edenMount->getObjectStore()->parseRootId(*parents->parent1_ref());
  if (params->hgRootManifest_ref().has_value()) {
    // The hg client has told us what the root manifest is.
    //
    // This is useful when a commit has just been created.  We won't be able to
    // ask the import helper to map the commit to its root manifest because it
    // won't know about the new commit until it reopens the repo.  Instead,
    // import the manifest for this commit directly.
    auto rootManifest = hash20FromThrift(*params->hgRootManifest_ref());
    edenMount->getObjectStore()
        ->getBackingStore()
        ->importManifestForRoot(parent1, rootManifest)
        .get();
  }
  edenMount->resetParent(parent1);
}

namespace {
/**
 * Convert the passed in SyncBehavior to a chrono type.
 *
 * When the SyncBehavior is unset, this default to a timeout of 60 seconds.
 */
std::chrono::seconds getSyncTimeout(const SyncBehavior& sync) {
  auto seconds = sync.syncTimeoutSeconds_ref().value_or(60);
  return std::chrono::seconds{seconds};
}

ImmediateFuture<folly::Unit> waitForPendingNotifications(
    const EdenMount& mount,
    std::chrono::seconds timeout) {
  if (timeout.count() == 0) {
    return folly::unit;
  }

  return mount.waitForPendingNotifications().semi().within(timeout);
}
} // namespace

folly::SemiFuture<folly::Unit>
EdenServiceHandler::semifuture_synchronizeWorkingCopy(
    std::unique_ptr<std::string> mountPoint,
    std::unique_ptr<SynchronizeWorkingCopyParams> params) {
  auto timeout = getSyncTimeout(*params->sync_ref());
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3, *mountPoint, timeout.count());
  auto mountPath = AbsolutePathPiece{*mountPoint};
  auto edenMount = server_->getMount(mountPath);

  return wrapImmediateFuture(
             std::move(helper),
             waitForPendingNotifications(*edenMount, timeout))
      .semi();
}

void EdenServiceHandler::getSHA1(
    vector<SHA1Result>& out,
    unique_ptr<string> mountPoint,
    unique_ptr<vector<string>> paths,
    std::unique_ptr<SyncBehavior> sync) {
  TraceBlock block("getSHA1");
  auto syncTimeout = getSyncTimeout(*sync);
  auto helper = INSTRUMENT_THRIFT_CALL(
      DBG3, *mountPoint, syncTimeout.count(), toLogArg(*paths));
  vector<ImmediateFuture<Hash20>> futures;
  auto mountPath = AbsolutePathPiece{*mountPoint};

  waitForPendingNotifications(*server_->getMount(mountPath), syncTimeout)
      .thenValue([&](auto&&) {
        for (const auto& path : *paths) {
          futures.emplace_back(getSHA1ForPathDefensively(
              mountPath, path, helper->getFetchContext()));
        }

        auto results = collectAll(std::move(futures)).get();
        for (auto& result : results) {
          out.emplace_back();
          SHA1Result& sha1Result = out.back();
          if (result.hasValue()) {
            sha1Result.sha1_ref() = thriftHash20(result.value());
          } else {
            sha1Result.error_ref() = newEdenError(result.exception());
          }
        }
      })
      .get();
}

ImmediateFuture<Hash20> EdenServiceHandler::getSHA1ForPathDefensively(
    AbsolutePathPiece mountPoint,
    StringPiece path,
    ObjectFetchContext& fetchContext) noexcept {
  return makeImmediateFutureWith(
      [&] { return getSHA1ForPath(mountPoint, path, fetchContext); });
}

ImmediateFuture<Hash20> EdenServiceHandler::getSHA1ForPath(
    AbsolutePathPiece mountPoint,
    StringPiece path,
    ObjectFetchContext& fetchContext) {
  if (path.empty()) {
    return ImmediateFuture<Hash20>(newEdenError(
        EINVAL,
        EdenErrorType::ARGUMENT_ERROR,
        "path cannot be the empty string"));
  }

  auto edenMount = server_->getMount(mountPoint);
  auto objectStore = edenMount->getObjectStore();
  auto relativePath = RelativePathPiece{path};
  return edenMount->getInodeOrTreeOrEntry(relativePath, fetchContext)
      .thenValue([relativePath, objectStore, &fetchContext](
                     const InodeOrTreeOrEntry& inodeOrTree) {
        return inodeOrTree.getSHA1(relativePath, objectStore, fetchContext);
      });
}

ImmediateFuture<EntryAttributes> EdenServiceHandler::getEntryAttributesForPath(
    AbsolutePathPiece mountPoint,
    StringPiece path,
    ObjectFetchContext& fetchContext) {
  if (path.empty()) {
    return ImmediateFuture<EntryAttributes>(newEdenError(
        EINVAL,
        EdenErrorType::ARGUMENT_ERROR,
        "path cannot be the empty string"));
  }

  try {
    auto edenMount = server_->getMount(mountPoint);
    auto objectStore = edenMount->getObjectStore();
    auto relativePath = RelativePathPiece{path};

    return edenMount->getInodeOrTreeOrEntry(relativePath, fetchContext)
        .thenValue([relativePath, objectStore, &fetchContext](
                       const InodeOrTreeOrEntry& inodeOrTree) {
          return inodeOrTree.getEntryAttributes(
              relativePath, objectStore, fetchContext);
        });
  } catch (const std::exception& e) {
    return ImmediateFuture<EntryAttributes>(
        newEdenError(EINVAL, EdenErrorType::ARGUMENT_ERROR, e.what()));
  }
}

void EdenServiceHandler::getBindMounts(
    std::vector<std::string>&,
    std::unique_ptr<std::string>) {
  // This deprecated method is only here until buck has swung through a
  // migration
}

void EdenServiceHandler::addBindMount(
    FOLLY_MAYBE_UNUSED std::unique_ptr<std::string> mountPoint,
    FOLLY_MAYBE_UNUSED std::unique_ptr<std::string> repoPath,
    FOLLY_MAYBE_UNUSED std::unique_ptr<std::string> targetPath) {
#ifndef _WIN32
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3, *mountPoint);
  auto mountPath = AbsolutePathPiece{*mountPoint};
  auto edenMount = server_->getMount(mountPath);

  edenMount
      ->addBindMount(
          RelativePathPiece{*repoPath},
          AbsolutePathPiece{*targetPath},
          helper->getFetchContext())
      .get();
#else
  NOT_IMPLEMENTED();
#endif
}

void EdenServiceHandler::removeBindMount(
    FOLLY_MAYBE_UNUSED std::unique_ptr<std::string> mountPoint,
    FOLLY_MAYBE_UNUSED std::unique_ptr<std::string> repoPath) {
#ifndef _WIN32
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3, *mountPoint);
  auto mountPath = AbsolutePathPiece{*mountPoint};
  auto edenMount = server_->getMount(mountPath);

  edenMount->removeBindMount(RelativePathPiece{*repoPath}).get();
#else
  NOT_IMPLEMENTED();
#endif
}

void EdenServiceHandler::getCurrentJournalPosition(
    JournalPosition& out,
    std::unique_ptr<std::string> mountPoint) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3, *mountPoint);
  auto mountPath = AbsolutePathPiece{*mountPoint};
  auto edenMount = server_->getMount(mountPath);
  auto latest = edenMount->getJournal().getLatest();

  *out.mountGeneration_ref() = edenMount->getMountGeneration();
  if (latest) {
    out.sequenceNumber_ref() = latest->sequenceID;
    out.snapshotHash_ref() =
        edenMount->getObjectStore()->renderRootId(latest->toHash);
  } else {
    out.sequenceNumber_ref() = 0;
    out.snapshotHash_ref() =
        edenMount->getObjectStore()->renderRootId(RootId{});
  }
}

apache::thrift::ServerStream<JournalPosition>
EdenServiceHandler::subscribeStreamTemporary(
    std::unique_ptr<std::string> mountPoint) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3, *mountPoint);
  auto mountPath = AbsolutePathPiece{*mountPoint};
  auto edenMount = server_->getMount(mountPath);

  // We need a weak ref on the mount because the thrift stream plumbing
  // may outlive the mount point
  std::weak_ptr<EdenMount> weakMount(edenMount);

  // We'll need to pass the subscriber id to both the disconnect
  // and change callbacks.  We can't know the id until after we've
  // created them both, so we need to share an optional id between them.
  auto handle = std::make_shared<std::optional<Journal::SubscriberId>>();
  auto disconnected = std::make_shared<std::atomic<bool>>(false);

  // This is called when the subscription channel is torn down
  auto onDisconnect = [weakMount, handle, disconnected] {
    XLOG(INFO) << "streaming client disconnected";
    auto mount = weakMount.lock();
    if (mount) {
      disconnected->store(true);
      mount->getJournal().cancelSubscriber(handle->value());
    }
  };

  // Set up the actual publishing instance
  auto streamAndPublisher =
      apache::thrift::ServerStream<JournalPosition>::createPublisher(
          std::move(onDisconnect));

  // A little wrapper around the StreamPublisher.
  // This is needed because the destructor for StreamPublisherState
  // triggers a FATAL if the stream has not been completed.
  // We don't have an easy way to trigger this outside of just calling
  // it in a destructor, so that's what we do here.
  struct Publisher {
    apache::thrift::ServerStreamPublisher<JournalPosition> publisher;
    std::shared_ptr<std::atomic<bool>> disconnected;

    explicit Publisher(
        apache::thrift::ServerStreamPublisher<JournalPosition> publisher,
        std::shared_ptr<std::atomic<bool>> disconnected)
        : publisher(std::move(publisher)),
          disconnected(std::move(disconnected)) {}

    ~Publisher() {
      // We have to send an exception as part of the completion, otherwise
      // thrift doesn't seem to notify the peer of the shutdown
      if (!disconnected->load()) {
        std::move(publisher).complete(
            folly::make_exception_wrapper<std::runtime_error>(
                "subscriber terminated"));
      }
    }
  };

  auto stream = std::make_shared<Publisher>(
      std::move(streamAndPublisher.second), std::move(disconnected));

  // Register onJournalChange with the journal subsystem, and assign
  // the subscriber id into the handle so that the callbacks can consume it.
  handle->emplace(edenMount->getJournal().registerSubscriber(
      [stream = std::move(stream)]() mutable {
        JournalPosition pos;
        // The value is intentionally undefined and should not be used. Instead,
        // the subscriber should call getCurrentJournalPosition or
        // getFilesChangedSince.
        stream->publisher.next(pos);
      }));

  return std::move(streamAndPublisher.first);
}

namespace {
TraceEventTimes thriftTraceEventTimes(const TraceEventBase& event) {
  using namespace std::chrono;

  TraceEventTimes times;
  times.timestamp_ref() =
      duration_cast<nanoseconds>(event.systemTime.time_since_epoch()).count();
  times.monotonic_time_ns_ref() =
      duration_cast<nanoseconds>(event.monotonicTime.time_since_epoch())
          .count();
  return times;
}

#ifndef _WIN32
RequestInfo thriftRequestInfo(pid_t pid, ProcessNameCache& processNameCache) {
  RequestInfo info;
  info.pid_ref() = pid;
  info.processName_ref().from_optional(processNameCache.getProcessName(pid));
  return info;
}
#endif

template <typename T>
class ThriftStreamPublisherOwner {
 public:
  explicit ThriftStreamPublisherOwner(
      apache::thrift::ServerStreamPublisher<T> publisher)
      : owner(true), publisher{std::move(publisher)} {}

  ThriftStreamPublisherOwner(ThriftStreamPublisherOwner&& that) noexcept
      : owner{std::exchange(that.owner, false)},
        publisher{std::move(that.publisher)} {}

  ThriftStreamPublisherOwner& operator=(ThriftStreamPublisherOwner&&) = delete;

  void next(T payload) const {
    if (owner) {
      publisher.next(std::move(payload));
    }
  }

  void next(folly::exception_wrapper ew) && {
    if (owner) {
      owner = false;
      std::move(publisher).complete(std::move(ew));
    }
  }

  // Destroying a publisher without calling complete() aborts the process, so
  // ensure complete() is called when this object is dropped.
  ~ThriftStreamPublisherOwner() {
    if (owner) {
      std::move(publisher).complete();
    }
  }

 private:
  bool owner;
  apache::thrift::ServerStreamPublisher<T> publisher;
};

} // namespace

#ifndef _WIN32

namespace {
FuseCall populateFuseCall(
    uint64_t unique,
    const FuseTraceEvent::RequestHeader& request,
    ProcessNameCache& processNameCache) {
  FuseCall fc;
  fc.opcode_ref() = request.opcode;
  fc.unique_ref() = unique;
  fc.nodeid_ref() = request.nodeid;
  fc.uid_ref() = request.uid;
  fc.gid_ref() = request.gid;
  fc.pid_ref() = request.pid;

  fc.opcodeName_ref() = fuseOpcodeName(request.opcode);
  fc.processName_ref().from_optional(
      processNameCache.getProcessName(request.pid));
  return fc;
}

NfsCall populateNfsCall(const NfsTraceEvent& event) {
  NfsCall nfsCall;
  nfsCall.xid_ref() = event.getXid();
  nfsCall.procNumber_ref() = event.getProcNumber();
  nfsCall.procName_ref() = nfsProcName(event.getProcNumber());
  return nfsCall;
}

/**
 * Returns true if event should not be traced.
 */

bool isEventMasked(
    int64_t eventCategoryMask,
    ProcessAccessLog::AccessType accessType) {
  using AccessType = ProcessAccessLog::AccessType;
  switch (accessType) {
    case AccessType::FsChannelRead:
      return 0 == (eventCategoryMask & streamingeden_constants::FS_EVENT_READ_);
    case AccessType::FsChannelWrite:
      return 0 ==
          (eventCategoryMask & streamingeden_constants::FS_EVENT_WRITE_);
    case AccessType::FsChannelOther:
    default:
      return 0 ==
          (eventCategoryMask & streamingeden_constants::FS_EVENT_OTHER_);
  }
}

bool isEventMasked(int64_t eventCategoryMask, const FuseTraceEvent& event) {
  return isEventMasked(
      eventCategoryMask, fuseOpcodeAccessType(event.getRequest().opcode));
}

bool isEventMasked(int64_t eventCategoryMask, const NfsTraceEvent& event) {
  return isEventMasked(
      eventCategoryMask, nfsProcAccessType(event.getProcNumber()));
}

} // namespace

#endif //!_WIN32

#ifdef _WIN32
PrjfsCall populatePrjfsCall(
    const PrjfsTraceCallType callType,
    const PrjfsTraceEvent::PrjfsOperationData& data) {
  PrjfsCall prjfsCall;
  prjfsCall.callType_ref() = callType;
  prjfsCall.commandId_ref() = data.commandId;
  prjfsCall.pid_ref() = data.pid;
  return prjfsCall;
}

PrjfsCall populatePrjfsCall(const PrjfsTraceEvent& event) {
  return populatePrjfsCall(event.getCallType(), event.getData());
}
#endif

apache::thrift::ServerStream<FsEvent> EdenServiceHandler::traceFsEvents(
    std::unique_ptr<std::string> mountPoint,
    int64_t eventCategoryMask) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3, *mountPoint);
  auto mountPath = AbsolutePathPiece{*mountPoint};
  auto edenMount = server_->getMount(mountPath);

  // Treat an empty bitset as an unfiltered stream. This is for clients that
  // predate the addition of the mask and for clients that don't care.
  // 0 would be meaningless anyway: it would never return any events.
  if (0 == eventCategoryMask) {
    eventCategoryMask = ~0;
  }

  struct Context {
    // While subscribed to FuseChannel's TraceBus, request detailed argument
    // strings.
    TraceDetailedArgumentsHandle argHandle;
#ifdef _WIN32
    TraceSubscriptionHandle<PrjfsTraceEvent> subHandle;
#else
    std::variant<
        TraceSubscriptionHandle<FuseTraceEvent>,
        TraceSubscriptionHandle<NfsTraceEvent>>
        subHandle;
#endif // _WIN32
  };

  auto context = std::make_shared<Context>();
#ifdef _WIN32
  auto prjfsChannel = edenMount->getPrjfsChannel()->getInner();
  if (prjfsChannel) {
    context->argHandle = prjfsChannel->traceDetailedArguments();
  } else {
    EDEN_BUG() << "tracing isn't supported yet for the "
               << edenMount->getCheckoutConfig()->getMountProtocol()
               << " filesystem type";
  }
#else
  auto* fuseChannel = edenMount->getFuseChannel();
  auto* nfsdChannel = edenMount->getNfsdChannel();
  if (fuseChannel) {
    context->argHandle = fuseChannel->traceDetailedArguments();
  } else if (nfsdChannel) {
    context->argHandle = nfsdChannel->traceDetailedArguments();
  } else {
    EDEN_BUG() << "tracing isn't supported yet for the "
               << edenMount->getCheckoutConfig()->getMountProtocol()
               << " filesystem type";
  }
#endif // _WIN32

  auto [serverStream, publisher] =
      apache::thrift::ServerStream<FsEvent>::createPublisher([context] {
        // on disconnect, release context and the TraceSubscriptionHandle
      });

#ifdef _WIN32
  if (prjfsChannel) {
    context->subHandle = prjfsChannel->getTraceBusPtr()->subscribeFunction(
        folly::to<std::string>("strace-", edenMount->getPath().basename()),
        [publisher = ThriftStreamPublisherOwner{std::move(publisher)},
         serverState =
             server_->getServerState()](const PrjfsTraceEvent& event) {
          FsEvent te;
          auto times = thriftTraceEventTimes(event);
          te.times_ref() = times;

          // Legacy timestamp fields.
          te.timestamp_ref() = *times.timestamp_ref();
          te.monotonic_time_ns_ref() = *times.monotonic_time_ns_ref();

          te.prjfsRequest_ref() = populatePrjfsCall(event);

          switch (event.getType()) {
            case PrjfsTraceEvent::START:
              te.type_ref() = FsEventType::START;
              if (auto& arguments = event.getArguments()) {
                te.arguments_ref() = *arguments;
              }
              break;
            case PrjfsTraceEvent::FINISH:
              te.type_ref() = FsEventType::FINISH;
              break;
          }

          te.requestInfo_ref() = RequestInfo{};

          publisher.next(te);
        });
  }
#else
  if (fuseChannel) {
    context->subHandle = fuseChannel->getTraceBus().subscribeFunction(
        folly::to<std::string>("strace-", edenMount->getPath().basename()),
        [publisher = ThriftStreamPublisherOwner{std::move(publisher)},
         serverState = server_->getServerState(),
         eventCategoryMask](const FuseTraceEvent& event) {
          if (isEventMasked(eventCategoryMask, event)) {
            return;
          }

          FsEvent te;
          auto times = thriftTraceEventTimes(event);
          te.times_ref() = times;

          // Legacy timestamp fields.
          te.timestamp_ref() = *times.timestamp_ref();
          te.monotonic_time_ns_ref() = *times.monotonic_time_ns_ref();

          te.fuseRequest_ref() = populateFuseCall(
              event.getUnique(),
              event.getRequest(),
              *serverState->getProcessNameCache());

          switch (event.getType()) {
            case FuseTraceEvent::START:
              te.type_ref() = FsEventType::START;
              if (auto& arguments = event.getArguments()) {
                te.arguments_ref() = *arguments;
              }
              break;
            case FuseTraceEvent::FINISH:
              te.type_ref() = FsEventType::FINISH;
              te.result_ref().from_optional(event.getResponseCode());
              break;
          }

          te.requestInfo_ref() = thriftRequestInfo(
              event.getRequest().pid, *serverState->getProcessNameCache());

          publisher.next(te);
        });
  } else if (nfsdChannel) {
    context->subHandle = nfsdChannel->getTraceBus().subscribeFunction(
        folly::to<std::string>("strace-", edenMount->getPath().basename()),
        [publisher = ThriftStreamPublisherOwner{std::move(publisher)},
         serverState = server_->getServerState(),
         eventCategoryMask](const NfsTraceEvent& event) {
          if (isEventMasked(eventCategoryMask, event)) {
            return;
          }

          FsEvent te;
          auto times = thriftTraceEventTimes(event);
          te.times_ref() = times;

          // Legacy timestamp fields.
          te.timestamp_ref() = *times.timestamp_ref();
          te.monotonic_time_ns_ref() = *times.monotonic_time_ns_ref();

          te.nfsRequest_ref() = populateNfsCall(event);

          switch (event.getType()) {
            case NfsTraceEvent::START:
              te.type_ref() = FsEventType::START;
              if (auto arguments = event.getArguments()) {
                te.arguments_ref() = arguments.value();
              }
              break;
            case NfsTraceEvent::FINISH:
              te.type_ref() = FsEventType::FINISH;
              break;
          }

          te.requestInfo_ref() = RequestInfo{};

          publisher.next(te);
        });
  }
#endif // _WIN32
  return std::move(serverStream);
}

apache::thrift::ServerStream<HgEvent> EdenServiceHandler::traceHgEvents(
    std::unique_ptr<std::string> mountPoint) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3, *mountPoint);
  auto mountPath = AbsolutePathPiece{*mountPoint};
  auto edenMount = server_->getMount(mountPath);
  auto backingStore = edenMount->getObjectStore()->getBackingStore();
  std::shared_ptr<HgQueuedBackingStore> hgBackingStore{nullptr};

  // TODO: remove these dynamic casts in favor of a QueryInterface method
  // BackingStore -> LocalStoreCachedBackingStore
  auto localStoreCachedBackingStore =
      std::dynamic_pointer_cast<LocalStoreCachedBackingStore>(backingStore);
  if (!localStoreCachedBackingStore) {
    // BackingStore -> HgQueuedBackingStore
    hgBackingStore =
        std::dynamic_pointer_cast<HgQueuedBackingStore>(backingStore);
  } else {
    // LocalStoreCachedBackingStore -> HgQueuedBackingStore
    hgBackingStore = std::dynamic_pointer_cast<HgQueuedBackingStore>(
        localStoreCachedBackingStore->getBackingStore());
  }

  if (!hgBackingStore) {
    // typeid() does not evaluate expressions
    auto& r = *backingStore.get();
    throw std::runtime_error(folly::to<std::string>(
        "mount ",
        mountPath,
        " must use HgQueuedBackingStore, type is ",
        typeid(r).name()));
  }

  struct Context {
    TraceSubscriptionHandle<HgImportTraceEvent> subHandle;
  };

  auto context = std::make_shared<Context>();

  auto [serverStream, publisher] =
      apache::thrift::ServerStream<HgEvent>::createPublisher([context] {
        // on disconnect, release context and the TraceSubscriptionHandle
      });

  context->subHandle = hgBackingStore->getTraceBus().subscribeFunction(
      folly::to<std::string>("hgtrace-", edenMount->getPath().basename()),
      [publisher = ThriftStreamPublisherOwner{std::move(publisher)},
       serverState =
           server_->getServerState()](const HgImportTraceEvent& event) {
        HgEvent te;
        te.times_ref() = thriftTraceEventTimes(event);
        switch (event.eventType) {
          case HgImportTraceEvent::QUEUE:
            te.eventType_ref() = HgEventType::QUEUE;
            break;
          case HgImportTraceEvent::START:
            te.eventType_ref() = HgEventType::START;
            break;
          case HgImportTraceEvent::FINISH:
            te.eventType_ref() = HgEventType::FINISH;
            break;
        }

        switch (event.resourceType) {
          case HgImportTraceEvent::BLOB:
            te.resourceType_ref() = HgResourceType::BLOB;
            break;
          case HgImportTraceEvent::TREE:
            te.resourceType_ref() = HgResourceType::TREE;
            break;
        }

        switch (event.importPriority) {
          case ImportPriorityKind::Low:
            te.importPriority_ref() = HgImportPriority::LOW;
            break;
          case ImportPriorityKind::Normal:
            te.importPriority_ref() = HgImportPriority::NORMAL;
            break;
          case ImportPriorityKind::High:
            te.importPriority_ref() = HgImportPriority::HIGH;
            break;
        }

        switch (event.importCause) {
          case ObjectFetchContext::Cause::Unknown:
            te.importCause_ref() = HgImportCause::UNKNOWN;
            break;
          case ObjectFetchContext::Cause::Fs:
            te.importCause_ref() = HgImportCause::FS;
            break;
          case ObjectFetchContext::Cause::Thrift:
            te.importCause_ref() = HgImportCause::THRIFT;
            break;
          case ObjectFetchContext::Cause::Prefetch:
            te.importCause_ref() = HgImportCause::PREFETCH;
            break;
        }

        te.unique_ref() = event.unique;

        te.manifestNodeId_ref() = event.manifestNodeId.toString();
        te.path_ref() = event.getPath();

        // TODO: trace requesting pid
        // te.requestInfo_ref() = thriftRequestInfo(pid);

        publisher.next(te);
      });

  return std::move(serverStream);
}

namespace {
void checkMountGeneration(
    const JournalPosition& position,
    const std::shared_ptr<EdenMount>& mount,
    std::string_view fieldName) {
  if (folly::to_unsigned(*position.mountGeneration()) !=
      mount->getMountGeneration()) {
    throw newEdenError(
        ERANGE,
        EdenErrorType::MOUNT_GENERATION_CHANGED,
        fieldName,
        ".mountGeneration does not match the current "
        "mountGeneration.  "
        "You need to compute a new basis for delta queries.");
  }
}

void publishFile(
    const folly::Synchronized<ThriftStreamPublisherOwner<ChangedFileResult>>&
        publisher,
    folly::StringPiece path,
    ScmFileStatus status,
    dtype_t type) {
  ChangedFileResult fileResult;
  fileResult.name() = path.str();
  fileResult.status() = status;
  fileResult.dtype() = static_cast<Dtype>(type);
  publisher.rlock()->next(std::move(fileResult));
}

class StreamingDiffCallback : public DiffCallback {
 public:
  explicit StreamingDiffCallback(
      std::shared_ptr<
          folly::Synchronized<ThriftStreamPublisherOwner<ChangedFileResult>>>
          publisher)
      : publisher_{std::move(publisher)} {}

  void ignoredPath(RelativePathPiece, dtype_t) override {}

  void addedPath(RelativePathPiece path, dtype_t type) override {
    publishFile(*publisher_, path.stringPiece(), ScmFileStatus::ADDED, type);
  }

  void removedPath(RelativePathPiece path, dtype_t type) override {
    publishFile(*publisher_, path.stringPiece(), ScmFileStatus::REMOVED, type);
  }

  void modifiedPath(RelativePathPiece path, dtype_t type) override {
    publishFile(*publisher_, path.stringPiece(), ScmFileStatus::MODIFIED, type);
  }

  void diffError(RelativePathPiece /*path*/, const folly::exception_wrapper& ew)
      override {
    auto publisher = std::move(*publisher_->wlock());
    std::move(publisher).next(newEdenError(ew));
  }

 private:
  std::shared_ptr<
      folly::Synchronized<ThriftStreamPublisherOwner<ChangedFileResult>>>
      publisher_;
};

} // namespace

apache::thrift::ResponseAndServerStream<ChangesSinceResult, ChangedFileResult>
EdenServiceHandler::streamChangesSince(
    std::unique_ptr<StreamChangesSinceParams> params) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3, *params->mountPoint_ref());
  auto mountPath = AbsolutePathPiece{*params->mountPoint_ref()};
  auto edenMount = server_->getMount(mountPath);
  const auto& fromPosition = *params->fromPosition_ref();

  // Streaming in Thrift can be done via a Stream Generator, or via a Stream
  // Publisher. We're using the latter here as the former can only be used with
  // coroutines which EdenFS hasn't been converted to. Generators also have the
  // property to be driven by the client, internally, Thrift will wait for the
  // client to have consumed an element before requesting more from the server.
  // Publishers on the other hand are driven by the server and are publishing
  // as fast as possible.
  //
  // What this means is that in the case where EdenFS can publish elements
  // faster than the client can read them, EdenFS's memory usage can grow
  // potentially unbounded.

  checkMountGeneration(fromPosition, edenMount, "fromPosition"sv);

  // The +1 is because the core merge stops at the item prior to
  // its limitSequence parameter and we want the changes *since*
  // the provided sequence number.
  auto summed = edenMount->getJournal().accumulateRange(
      *fromPosition.sequenceNumber_ref() + 1);

  ChangesSinceResult result;
  if (!summed) {
    // No changes, just return the fromPosition and an empty stream.
    result.toPosition_ref() = fromPosition;

    return {
        std::move(result),
        apache::thrift::ServerStream<ChangedFileResult>::createEmpty()};
  }

  if (summed->isTruncated) {
    throw newEdenError(
        EDOM,
        EdenErrorType::JOURNAL_TRUNCATED,
        "Journal entry range has been truncated.");
  }

  auto cancellationSource = std::make_shared<folly::CancellationSource>();
  auto [serverStream, publisher] =
      apache::thrift::ServerStream<ChangedFileResult>::createPublisher(
          [cancellationSource] { cancellationSource->requestCancellation(); });
  auto sharedPublisher = std::make_shared<
      folly::Synchronized<ThriftStreamPublisherOwner<ChangedFileResult>>>(
      ThriftStreamPublisherOwner{std::move(publisher)});

  RootIdCodec& rootIdCodec = *edenMount->getObjectStore();

  JournalPosition toPosition;
  toPosition.mountGeneration_ref() = edenMount->getMountGeneration();
  toPosition.sequenceNumber_ref() = summed->toSequence;
  toPosition.snapshotHash_ref() =
      rootIdCodec.renderRootId(summed->snapshotTransitions.back());
  result.toPosition_ref() = toPosition;

  for (auto& entry : summed->changedFilesInOverlay) {
    const auto& changeInfo = entry.second;

    ScmFileStatus status;
    if (!changeInfo.existedBefore && changeInfo.existedAfter) {
      status = ScmFileStatus::ADDED;
    } else if (changeInfo.existedBefore && !changeInfo.existedAfter) {
      status = ScmFileStatus::REMOVED;
    } else {
      status = ScmFileStatus::MODIFIED;
    }

    publishFile(
        *sharedPublisher,
        entry.first.stringPiece().str(),
        status,
        dtype_t::Unknown);
  }

  for (const auto& name : summed->uncleanPaths) {
    publishFile(
        *sharedPublisher,
        name.stringPiece().str(),
        ScmFileStatus::MODIFIED,
        dtype_t::Unknown);
  }

  if (summed->snapshotTransitions.size() > 1) {
    auto callback = std::make_shared<StreamingDiffCallback>(sharedPublisher);

    std::vector<ImmediateFuture<folly::Unit>> futures;
    for (auto rootIt = summed->snapshotTransitions.begin();
         std::next(rootIt) != summed->snapshotTransitions.end();
         ++rootIt) {
      const auto& from = *rootIt;
      const auto& to = *(rootIt + 1);

      futures.push_back(edenMount->diffBetweenRoots(
          from, to, cancellationSource->getToken(), callback.get()));
    }

    folly::futures::detachOn(
        server_->getServerState()->getThreadPool().get(),
        collectAllSafe(std::move(futures))
            // Make sure that the edenMount, callback, helper and
            // cancellationSource lives for the duration of the stream by
            // copying them.
            .thenTry(
                [edenMount,
                 sharedPublisher,
                 callback = std::move(callback),
                 helper = std::move(helper),
                 cancellationSource](
                    folly::Try<std::vector<folly::Unit>>&& result) mutable {
                  if (result.hasException()) {
                    auto publisher = std::move(*sharedPublisher->wlock());
                    std::move(publisher).next(
                        newEdenError(std::move(result).exception()));
                  }
                })
            .semi());
  }

  return {std::move(result), std::move(serverStream)};
}

void EdenServiceHandler::getFilesChangedSince(
    FileDelta& out,
    std::unique_ptr<std::string> mountPoint,
    std::unique_ptr<JournalPosition> fromPosition) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3, *mountPoint);
  auto mountPath = AbsolutePathPiece{*mountPoint};
  auto edenMount = server_->getMount(mountPath);

  checkMountGeneration(*fromPosition, edenMount, "fromPosition"sv);

  // The +1 is because the core merge stops at the item prior to
  // its limitSequence parameter and we want the changes *since*
  // the provided sequence number.
  auto summed = edenMount->getJournal().accumulateRange(
      *fromPosition->sequenceNumber_ref() + 1);

  // We set the default toPosition to be where we where if summed is null
  out.toPosition_ref()->sequenceNumber_ref() =
      *fromPosition->sequenceNumber_ref();
  out.toPosition_ref()->snapshotHash_ref() = *fromPosition->snapshotHash_ref();
  out.toPosition_ref()->mountGeneration_ref() = edenMount->getMountGeneration();

  out.fromPosition_ref() = *out.toPosition_ref();

  if (summed) {
    if (summed->isTruncated) {
      throw newEdenError(
          EDOM,
          EdenErrorType::JOURNAL_TRUNCATED,
          "Journal entry range has been truncated.");
    }

    RootIdCodec& rootIdCodec = *edenMount->getObjectStore();

    out.toPosition_ref()->sequenceNumber_ref() = summed->toSequence;
    out.toPosition_ref()->snapshotHash_ref() =
        rootIdCodec.renderRootId(summed->snapshotTransitions.back());
    out.toPosition_ref()->mountGeneration_ref() =
        edenMount->getMountGeneration();

    out.fromPosition_ref()->sequenceNumber_ref() = summed->fromSequence;
    out.fromPosition_ref()->snapshotHash_ref() =
        rootIdCodec.renderRootId(summed->snapshotTransitions.front());
    out.fromPosition_ref()->mountGeneration_ref() =
        *out.toPosition_ref()->mountGeneration_ref();

    for (const auto& entry : summed->changedFilesInOverlay) {
      auto& path = entry.first;
      auto& changeInfo = entry.second;
      if (changeInfo.isNew()) {
        out.createdPaths_ref()->emplace_back(path.stringPiece().str());
      } else {
        out.changedPaths_ref()->emplace_back(path.stringPiece().str());
      }
    }

    for (auto& path : summed->uncleanPaths) {
      out.uncleanPaths_ref()->emplace_back(path.stringPiece().str());
    }

    out.snapshotTransitions_ref()->reserve(summed->snapshotTransitions.size());
    for (auto& hash : summed->snapshotTransitions) {
      out.snapshotTransitions_ref()->push_back(rootIdCodec.renderRootId(hash));
    }
  }
}

void EdenServiceHandler::setJournalMemoryLimit(
    std::unique_ptr<PathString> mountPoint,
    int64_t limit) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3, *mountPoint);
  auto mountPath = AbsolutePathPiece{*mountPoint};
  auto edenMount = server_->getMount(mountPath);
  if (limit < 0) {
    throw newEdenError(
        EINVAL,
        EdenErrorType::ARGUMENT_ERROR,
        "memory limit must be non-negative");
  }
  edenMount->getJournal().setMemoryLimit(static_cast<size_t>(limit));
}

int64_t EdenServiceHandler::getJournalMemoryLimit(
    std::unique_ptr<PathString> mountPoint) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3, *mountPoint);
  auto mountPath = AbsolutePathPiece{*mountPoint};
  auto edenMount = server_->getMount(mountPath);
  return static_cast<int64_t>(edenMount->getJournal().getMemoryLimit());
}

void EdenServiceHandler::flushJournal(std::unique_ptr<PathString> mountPoint) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3, *mountPoint);
  auto mountPath = AbsolutePathPiece{*mountPoint};
  auto edenMount = server_->getMount(mountPath);
  edenMount->getJournal().flush();
}

void EdenServiceHandler::debugGetRawJournal(
    DebugGetRawJournalResponse& out,
    std::unique_ptr<DebugGetRawJournalParams> params) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3, *params->mountPoint_ref());
  auto mountPath = AbsolutePathPiece{*params->mountPoint_ref()};
  auto edenMount = server_->getMount(mountPath);
  auto mountGeneration = static_cast<ssize_t>(edenMount->getMountGeneration());

  std::optional<size_t> limitopt = std::nullopt;
  if (auto limit = params->limit_ref()) {
    limitopt = static_cast<size_t>(*limit);
  }

  out.allDeltas_ref() = edenMount->getJournal().getDebugRawJournalInfo(
      *params->fromSequenceNumber_ref(),
      limitopt,
      mountGeneration,
      *edenMount->getObjectStore());
}

folly::SemiFuture<std::unique_ptr<std::vector<EntryInformationOrError>>>
EdenServiceHandler::semifuture_getEntryInformation(
    std::unique_ptr<std::string> mountPoint,
    std::unique_ptr<std::vector<std::string>> paths,
    std::unique_ptr<SyncBehavior> sync) {
  auto syncTimeout = getSyncTimeout(*sync);
  auto helper = INSTRUMENT_THRIFT_CALL(
      DBG3, *mountPoint, syncTimeout.count(), toLogArg(*paths));
  auto mountPath = AbsolutePathPiece{*mountPoint};
  auto edenMount = server_->getMount(mountPath);
  auto rootInode = edenMount->getRootInode();
  auto objectStore = edenMount->getObjectStore();
  auto& fetchContext = helper->getFetchContext();

  return wrapImmediateFuture(
             std::move(helper),
             waitForPendingNotifications(*edenMount, syncTimeout)
                 .thenValue([rootInode = std::move(rootInode),
                             paths = std::move(paths),
                             objectStore,
                             &fetchContext](auto&&) {
                   return collectAll(applyToInodeOrTreeOrEntry(
                                         rootInode,
                                         *paths,
                                         [](const InodeOrTreeOrEntry& inode) {
                                           return inode.getDtype();
                                         },
                                         objectStore,
                                         fetchContext))
                       .deferValue([](vector<Try<dtype_t>> done) {
                         auto out = std::make_unique<
                             vector<EntryInformationOrError>>();
                         out->reserve(done.size());
                         for (auto& item : done) {
                           EntryInformationOrError result;
                           if (item.hasException()) {
                             result.error_ref() =
                                 newEdenError(item.exception());
                           } else {
                             EntryInformation info;
                             info.dtype_ref() =
                                 static_cast<Dtype>(item.value());
                             result.info_ref() = info;
                           }
                           out->emplace_back(std::move(result));
                         }
                         return out;
                       });
                 }))
      .semi();
}

folly::SemiFuture<std::unique_ptr<std::vector<FileInformationOrError>>>
EdenServiceHandler::semifuture_getFileInformation(
    std::unique_ptr<std::string> mountPoint,
    std::unique_ptr<std::vector<std::string>> paths,
    std::unique_ptr<SyncBehavior> sync) {
  auto syncTimeout = getSyncTimeout(*sync);
  auto helper = INSTRUMENT_THRIFT_CALL(
      DBG3, *mountPoint, syncTimeout.count(), toLogArg(*paths));
  auto mountPath = AbsolutePathPiece{*mountPoint};
  auto edenMount = server_->getMount(mountPath);
  auto rootInode = edenMount->getRootInode();
  auto objectStore = edenMount->getObjectStore();
  auto& fetchContext = helper->getFetchContext();
  auto lastCheckoutTime = edenMount->getLastCheckoutTime().toTimespec();

  return wrapImmediateFuture(
             std::move(helper),
             waitForPendingNotifications(*edenMount, syncTimeout)
                 .thenValue([rootInode = std::move(rootInode),
                             paths = std::move(paths),
                             lastCheckoutTime,
                             objectStore,
                             &fetchContext](auto&&) {
                   return collectAll(
                              applyToInodeOrTreeOrEntry(
                                  rootInode,
                                  *paths,
                                  [lastCheckoutTime,
                                   objectStore,
                                   &fetchContext](
                                      const InodeOrTreeOrEntry& inode) {
                                    return inode
                                        .stat(
                                            lastCheckoutTime,
                                            objectStore,
                                            fetchContext)
                                        .thenValue([](struct stat st) {
                                          FileInformation info;
                                          info.size_ref() = st.st_size;
                                          auto ts = stMtime(st);
                                          info.mtime_ref()->seconds_ref() =
                                              ts.tv_sec;
                                          info.mtime_ref()->nanoSeconds_ref() =
                                              ts.tv_nsec;
                                          info.mode_ref() = st.st_mode;

                                          FileInformationOrError result;
                                          result.info_ref() = info;

                                          return result;
                                        })
                                        .semi();
                                  },
                                  objectStore,
                                  fetchContext))
                       .deferValue([](vector<Try<FileInformationOrError>>&&
                                          done) {
                         auto out =
                             std::make_unique<vector<FileInformationOrError>>();
                         out->reserve(done.size());
                         for (auto& item : done) {
                           if (item.hasException()) {
                             FileInformationOrError result;
                             result.error_ref() =
                                 newEdenError(item.exception());
                             out->emplace_back(std::move(result));
                           } else {
                             out->emplace_back(item.value());
                           }
                         }
                         return out;
                       });
                 }))
      .semi();
}

#define ATTR_BITMASK(req, attr) \
  ((req) & static_cast<uint64_t>((FileAttributes::attr)))

namespace {
SourceControlType entryTypeToThriftType(TreeEntryType type) {
  switch (type) {
    case TreeEntryType::TREE:
      return SourceControlType::TREE;
    case TreeEntryType::REGULAR_FILE:
      return SourceControlType::REGULAR_FILE;
    case TreeEntryType::EXECUTABLE_FILE:
      return SourceControlType::EXECUTABLE_FILE;
    case TreeEntryType::SYMLINK:
      return SourceControlType::SYMLINK;
    default:
      throw std::system_error(EINVAL, std::generic_category());
  }
}
} // namespace

folly::SemiFuture<std::unique_ptr<GetAttributesFromFilesResult>>
EdenServiceHandler::semifuture_getAttributesFromFiles(
    std::unique_ptr<GetAttributesFromFilesParams> params) {
  auto mountPoint = params->get_mountPoint();
  auto mountPath = AbsolutePathPiece{mountPoint};
  auto paths = params->get_paths();
  auto reqBitmask = params->get_requestedAttributes();
  auto syncTimeout = getSyncTimeout(*params->sync_ref());
  // Get requested attributes for each path
  auto helper = INSTRUMENT_THRIFT_CALL(
      DBG3, mountPoint, syncTimeout.count(), toLogArg(paths));
  auto& fetchContext = helper->getFetchContext();

  return wrapImmediateFuture(
             std::move(helper),
             waitForPendingNotifications(
                 *server_->getMount(mountPath), syncTimeout)
                 .thenValue([this,
                             paths = std::move(paths),
                             &fetchContext,
                             mountPath = mountPath.copy(),
                             reqBitmask](auto&&) mutable {
                   vector<ImmediateFuture<EntryAttributes>> futures;
                   for (const auto& p : paths) {
                     futures.emplace_back(
                         getEntryAttributesForPath(mountPath, p, fetchContext));
                   }

                   // Collect all futures into a single tuple
                   return facebook::eden::collectAll(std::move(futures))
                       .thenValue([paths = std::move(paths), reqBitmask](
                                      std::vector<folly::Try<EntryAttributes>>&&
                                          allRes) {
                         auto res =
                             std::make_unique<GetAttributesFromFilesResult>();
                         auto sizeRequested =
                             ATTR_BITMASK(reqBitmask, FILE_SIZE);
                         auto sha1Requested =
                             ATTR_BITMASK(reqBitmask, SHA1_HASH);
                         auto typeRequested =
                             ATTR_BITMASK(reqBitmask, SOURCE_CONTROL_TYPE);

                         size_t index = 0;
                         for (const auto& tryAttributes : allRes) {
                           FileAttributeDataOrError file_res;
                           // check for exceptions. if found, return EdenError
                           // early
                           if (tryAttributes.hasException()) {
                             file_res.error_ref() =
                                 newEdenError(tryAttributes.exception());
                           } else { /* No exceptions, fill in data */
                             FileAttributeData file_data;
                             const auto& attributes = tryAttributes.value();

                             // clients rely on these top level exceptions to
                             // detect symlinks and directories.
                             // TODO(kmancini): When Buck2 migrates to our
                             // explicit type information, we can get shape up
                             // this API better.
                             if (attributes.sha1.hasException()) {
                               file_res.error_ref() =
                                   newEdenError(attributes.sha1.exception());
                             } else if (attributes.size.hasException()) {
                               file_res.error_ref() =
                                   newEdenError(attributes.size.exception());
                             } else if (attributes.type.hasException()) {
                               file_res.error_ref() =
                                   newEdenError(attributes.type.exception());
                             } else {
                               // Only fill in requested fields
                               if (sha1Requested) {
                                 file_data.sha1_ref() =
                                     thriftHash20(attributes.sha1.value());
                               }
                               if (sizeRequested) {
                                 file_data.fileSize_ref() =
                                     attributes.size.value();
                               }
                               if (typeRequested) {
                                 file_data.type_ref() = entryTypeToThriftType(
                                     attributes.type.value());
                               }
                               file_res.data_ref() = file_data;
                             }
                           }
                           res->res_ref()->emplace_back(file_res);
                           ++index;
                         }
                         return res;
                       });
                 }))
      .semi();
}

folly::SemiFuture<std::unique_ptr<SetPathObjectIdResult>>
EdenServiceHandler::semifuture_setPathObjectId(
    std::unique_ptr<SetPathObjectIdParams> params) {
#ifndef _WIN32
  auto mountPoint = params->get_mountPoint();
  auto mountPath = AbsolutePathPiece{mountPoint};
  auto edenMount = server_->getMount(mountPath);
  // TODO: This function should operate with ObjectId instead of RootId.
  auto repoPath = params->get_path();
  auto parsedRootId =
      edenMount->getObjectStore()->parseRootId(params->get_objectId());
  auto helper = INSTRUMENT_THRIFT_CALL(
      DBG1, mountPoint, repoPath, parsedRootId, params->get_type());

  auto& fetchContext = helper->getFetchContext();
  if (auto requestInfo = params->requestInfo_ref()) {
    fetchContext.updateRequestInfo(std::move(*requestInfo));
  }
  return wrapImmediateFuture(
             std::move(helper),
             edenMount
                 ->setPathObjectId(
                     RelativePathPiece{repoPath},
                     parsedRootId,
                     params->get_type(),
                     params->get_mode(),
                     fetchContext)
                 .thenValue([](auto&& resultAndTimes) {
                   return std::make_unique<SetPathObjectIdResult>(
                       std::move(resultAndTimes.result));
                 }))
      .semi();
#else
  (void)params;
  NOT_IMPLEMENTED();
#endif
}

folly::SemiFuture<folly::Unit> EdenServiceHandler::semifuture_removeRecursively(
    std::unique_ptr<RemoveRecursivelyParams> params) {
  auto mountPoint = params->get_mountPoint();
  auto repoPath = params->get_path();
  auto syncTimeout = getSyncTimeout(*params->sync_ref());

  auto helper = INSTRUMENT_THRIFT_CALL(DBG2, mountPoint, repoPath);
  auto mountPath = AbsolutePathPiece{mountPoint};
  auto edenMount = server_->getMount(mountPath);

  auto relativePath = RelativePath{repoPath};
  auto& fetchContext = helper->getFetchContext();

  TreeInodePtr inode = edenMount->getInodeSlow(relativePath, fetchContext)
                           .get()
                           ->getParentRacy();
  return wrapImmediateFuture(
             std::move(helper),
             waitForPendingNotifications(*edenMount, syncTimeout)
                 .thenValue([inode = std::move(inode),
                             relativePath = std::move(relativePath),
                             &fetchContext](auto&&) {
                   return inode->removeRecursively(
                       relativePath.basename(),
                       InvalidationRequired::Yes,
                       fetchContext);
                 }))
      .semi();
}

namespace {
folly::Future<std::unique_ptr<Glob>> detachIfBackgrounded(
    folly::Future<std::unique_ptr<Glob>> globFuture,
    const std::shared_ptr<ServerState>& serverState,
    bool background) {
  if (!background) {
    return globFuture;
  } else {
    folly::futures::detachOn(
        serverState->getThreadPool().get(), std::move(globFuture).semi());
    return folly::makeFuture<std::unique_ptr<Glob>>(std::make_unique<Glob>());
  }
}
} // namespace

folly::Future<std::unique_ptr<Glob>>
EdenServiceHandler::future_predictiveGlobFiles(
    std::unique_ptr<GlobParams> params) {
#ifdef EDEN_HAVE_USAGE_SERVICE
  ThriftGlobImpl globber{*params};
  auto helper = INSTRUMENT_THRIFT_CALL_WITH_FUNCTION_NAME_AND_PID(
      DBG3,
      __func__,
      getAndRegisterClientPid(),
      *params->mountPoint_ref(),
      globber.logString());

  auto& mountPoint = *params->mountPoint_ref();
  /* set predictive glob fetch parameters */
  // if numResults is not specified, use default predictivePrefetchProfileSize
  auto& serverState = server_->getServerState();
  auto numResults =
      serverState->getEdenConfig()->predictivePrefetchProfileSize.getValue();
  // if user is not specified, get user info from the server state
  auto user = folly::StringPiece{serverState->getUserInfo().getUsername()};
  auto edenMount = server_->getMount(AbsolutePathPiece{mountPoint});
  auto backingStore = edenMount->getObjectStore()->getBackingStore();
  // if repo is not specified, get repository name from the backingstore
  auto repo_optional = backingStore->getRepoName();
  if (repo_optional == std::nullopt) {
    // typeid() does not evaluate expressions
    auto& r = *backingStore.get();
    throw std::runtime_error(folly::to<std::string>(
        "mount must use HgQueuedBackingStore, type is ", typeid(r).name()));
  }

  auto repo = repo_optional.value();
  auto os = getOperatingSystemName();

  // sandcastleAlias, startTime, and endTime are optional parameters
  std::optional<std::string> sandcastleAlias;
  std::optional<uint64_t> startTime;
  std::optional<uint64_t> endTime;
  // check if this is a sandcastle job (getenv will return nullptr if the env
  // variable is not set)
  auto scAliasEnv = std::getenv("SANDCASTLE_ALIAS");
  sandcastleAlias = scAliasEnv ? std::make_optional(std::string(scAliasEnv))
                               : sandcastleAlias;

  // check specified predictive parameters
  const auto& predictiveGlob = params->predictiveGlob_ref();
  if (predictiveGlob.has_value()) {
    numResults = predictiveGlob->numTopDirectories_ref().value_or(numResults);
    user = predictiveGlob->user_ref().has_value()
        ? predictiveGlob->user_ref().value()
        : user;
    repo = predictiveGlob->repo_ref().has_value()
        ? predictiveGlob->repo_ref().value()
        : repo;
    os = predictiveGlob->os_ref().has_value() ? predictiveGlob->os_ref().value()
                                              : os;
    startTime = predictiveGlob->startTime_ref().has_value()
        ? predictiveGlob->startTime_ref().value()
        : startTime;
    endTime = predictiveGlob->endTime_ref().has_value()
        ? predictiveGlob->endTime_ref().value()
        : endTime;
  }

  auto& fetchContext = helper->getPrefetchFetchContext();
  bool background = *params->background();

  auto future =
      spServiceEndpoint_
          ->getTopUsedDirs(
              user, repo, numResults, os, startTime, endTime, sandcastleAlias)
          .thenValue([globber = std::move(globber),
                      edenMount = std::move(edenMount),
                      serverState,
                      &fetchContext](std::vector<std::string>&& globs) mutable {
            return globber.glob(edenMount, serverState, globs, fetchContext);
          })
          .thenError([](folly::exception_wrapper&& ew) {
            XLOG(ERR) << "Error fetching predictive file globs: "
                      << folly::exceptionStr(ew);
            return makeFuture<std::unique_ptr<Glob>>(std::move(ew));
          })
          .ensure(
              [params = std::move(params), helper = std::move(helper)]() {});
  return detachIfBackgrounded(std::move(future), serverState, background);
#else // !EDEN_HAVE_USAGE_SERVICE
  (void)params;
  NOT_IMPLEMENTED();
#endif // !EDEN_HAVE_USAGE_SERVICE
}

folly::Future<std::unique_ptr<Glob>> EdenServiceHandler::future_globFiles(
    std::unique_ptr<GlobParams> params) {
  ThriftGlobImpl globber{*params};
  auto helper = INSTRUMENT_THRIFT_CALL_WITH_FUNCTION_NAME_AND_PID(
      DBG3,
      __func__,
      getAndRegisterClientPid(),
      *params->mountPoint_ref(),
      toLogArg(*params->globs_ref()),
      globber.logString());
  auto& context = helper->getPrefetchFetchContext();
  auto globFut =
      globber
          .glob(
              server_->getMount(AbsolutePathPiece{*params->mountPoint_ref()}),
              server_->getServerState(),
              *params->globs_ref(),
              context)
          .ensure([helper = std::move(helper)] {});
  return detachIfBackgrounded(
      std::move(globFut), server_->getServerState(), *params->background());
}

folly::Future<Unit> EdenServiceHandler::future_chown(
    FOLLY_MAYBE_UNUSED std::unique_ptr<std::string> mountPoint,
    FOLLY_MAYBE_UNUSED int32_t uid,
    FOLLY_MAYBE_UNUSED int32_t gid) {
#ifndef _WIN32
  auto mountPath = AbsolutePathPiece{*mountPoint};
  auto edenMount = server_->getMount(mountPath);
  return edenMount->chown(uid, gid);
#else
  NOT_IMPLEMENTED();
#endif // !_WIN32
}

folly::Future<std::unique_ptr<GetScmStatusResult>>
EdenServiceHandler::future_getScmStatusV2(
    unique_ptr<GetScmStatusParams> params) {
  auto* context = getRequestContext();

  auto helper = INSTRUMENT_THRIFT_CALL(
      DBG2,
      *params->mountPoint_ref(),
      folly::to<string>("commitHash=", logHash(*params->commit_ref())),
      folly::to<string>("listIgnored=", *params->listIgnored_ref()));

  auto mountPath = AbsolutePathPiece{*params->mountPoint_ref()};
  auto mount = server_->getMount(mountPath);
  auto rootId = mount->getObjectStore()->parseRootId(*params->commit_ref());
  const auto& enforceParents = server_->getServerState()
                                   ->getReloadableConfig()
                                   ->getEdenConfig()
                                   ->enforceParents.getValue();
  return wrapFuture(
      std::move(helper),
      mount
          ->diff(
              rootId,
              context->getConnectionContext()->getCancellationToken(),
              *params->listIgnored_ref(),
              enforceParents)
          .thenValue([this, mount](std::unique_ptr<ScmStatus>&& status) {
            auto result = std::make_unique<GetScmStatusResult>();
            result->status_ref() = std::move(*status);
            result->version_ref() = server_->getVersion();
            return result;
          }));
}

folly::Future<std::unique_ptr<ScmStatus>>
EdenServiceHandler::future_getScmStatus(
    unique_ptr<string> mountPoint,
    bool listIgnored,
    unique_ptr<string> commitHash) {
  auto* context = getRequestContext();
  auto helper = INSTRUMENT_THRIFT_CALL(
      DBG2,
      *mountPoint,
      folly::to<string>("listIgnored=", listIgnored ? "true" : "false"),
      folly::to<string>("commitHash=", logHash(*commitHash)));

  // Unlike getScmStatusV2(), this older getScmStatus() call does not enforce
  // that the caller specified the current commit.  In the future we might
  // want to enforce that even for this call, if we confirm that all existing
  // callers of this method can deal with the error.
  auto mountPath = AbsolutePathPiece{*mountPoint};
  auto mount = server_->getMount(mountPath);
  auto hash = mount->getObjectStore()->parseRootId(*commitHash);
  return wrapFuture(
      std::move(helper),
      mount->diff(
          hash,
          context->getConnectionContext()->getCancellationToken(),
          listIgnored,
          /*enforceCurrentParent=*/false));
}

folly::SemiFuture<unique_ptr<ScmStatus>>
EdenServiceHandler::semifuture_getScmStatusBetweenRevisions(
    unique_ptr<string> mountPoint,
    unique_ptr<string> oldHash,
    unique_ptr<string> newHash) {
  auto* context = getRequestContext();
  auto helper = INSTRUMENT_THRIFT_CALL(
      DBG2,
      *mountPoint,
      folly::to<string>("oldHash=", logHash(*oldHash)),
      folly::to<string>("newHash=", logHash(*newHash)));
  auto mountPath = AbsolutePathPiece{*mountPoint};
  auto mount = server_->getMount(mountPath);

  auto callback = std::make_unique<ScmStatusDiffCallback>();
  auto diffFuture = mount->diffBetweenRoots(
      mount->getObjectStore()->parseRootId(*oldHash),
      mount->getObjectStore()->parseRootId(*newHash),
      context->getConnectionContext()->getCancellationToken(),
      callback.get());
  return wrapImmediateFuture(
             std::move(helper),
             std::move(diffFuture)
                 .thenValue([callback = std::move(callback)](auto&&) {
                   return std::make_unique<ScmStatus>(
                       callback->extractStatus());
                 }))
      .semi();
}

void EdenServiceHandler::debugGetScmTree(
    vector<ScmTreeEntry>& entries,
    unique_ptr<string> mountPoint,
    unique_ptr<string> idStr,
    bool localStoreOnly) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG2, *mountPoint, logHash(*idStr));
  auto mountPath = AbsolutePathPiece{*mountPoint};
  auto edenMount = server_->getMount(mountPath);
  auto id = edenMount->getObjectStore()->parseObjectId(*idStr);

  std::shared_ptr<const Tree> tree;
  auto store = edenMount->getObjectStore();
  if (localStoreOnly) {
    auto localStore = store->getLocalStore();
    tree = localStore->getTree(id).get();
  } else {
    tree = store->getTree(id, helper->getFetchContext()).get();
  }

  if (!tree) {
    throw newEdenError(
        ENOENT,
        EdenErrorType::POSIX_ERROR,
        "no tree found for id ",
        edenMount->getObjectStore()->renderObjectId(id));
  }

  for (const auto& entry : *tree) {
    const auto& [name, treeEntry] = entry;
    entries.emplace_back();
    auto& out = entries.back();
    out.name_ref() = name.stringPiece().str();
    out.mode_ref() = modeFromTreeEntryType(treeEntry.getType());
    out.id_ref() =
        edenMount->getObjectStore()->renderObjectId(treeEntry.getHash());
  }
}

void EdenServiceHandler::debugGetScmBlob(
    string& data,
    unique_ptr<string> mountPoint,
    unique_ptr<string> idStr,
    bool localStoreOnly) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG2, *mountPoint, logHash(*idStr));
  auto mountPath = AbsolutePathPiece{*mountPoint};
  auto edenMount = server_->getMount(mountPath);
  auto id = edenMount->getObjectStore()->parseObjectId(*idStr);

  std::shared_ptr<const Blob> blob;
  auto store = edenMount->getObjectStore();
  if (localStoreOnly) {
    auto localStore = store->getLocalStore();
    blob = localStore->getBlob(id).get();
  } else {
    blob = store->getBlob(id, helper->getFetchContext()).get();
  }

  if (!blob) {
    throw newEdenError(
        ENOENT,
        EdenErrorType::POSIX_ERROR,
        "no blob found for id ",
        edenMount->getObjectStore()->renderObjectId(id));
  }
  auto dataBuf = blob->getContents().cloneCoalescedAsValue();
  data.assign(reinterpret_cast<const char*>(dataBuf.data()), dataBuf.length());
}

void EdenServiceHandler::debugGetScmBlobMetadata(
    ScmBlobMetadata& result,
    unique_ptr<string> mountPoint,
    unique_ptr<string> idStr,
    bool localStoreOnly) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG2, *mountPoint, logHash(*idStr));
  auto mountPath = AbsolutePathPiece{*mountPoint};
  auto edenMount = server_->getMount(mountPath);
  auto id = edenMount->getObjectStore()->parseObjectId(*idStr);

  std::optional<BlobMetadata> metadata;
  auto store = edenMount->getObjectStore();
  if (localStoreOnly) {
    auto localStore = store->getLocalStore();
    metadata = localStore->getBlobMetadata(id).get();
  } else {
    auto& fetchContext = helper->getFetchContext();
    auto sha1 = store->getBlobSha1(id, fetchContext).get();
    auto size = store->getBlobSize(id, fetchContext).get();
    metadata.emplace(sha1, size);
  }

  if (!metadata.has_value()) {
    throw newEdenError(
        ENOENT,
        EdenErrorType::POSIX_ERROR,
        "no blob metadata found for id ",
        id);
  }
  result.size_ref() = metadata->size;
  result.contentsSha1_ref() = thriftHash20(metadata->sha1);
}

namespace {
class InodeStatusCallbacks : public TraversalCallbacks {
 public:
  explicit InodeStatusCallbacks(
      EdenMount* mount,
      int64_t flags,
      std::vector<TreeInodeDebugInfo>& results)
      : mount_{mount}, flags_{flags}, results_{results} {}

  void visitTreeInode(
      RelativePathPiece path,
      InodeNumber ino,
      const std::optional<ObjectId>& hash,
      uint64_t fsRefcount,
      const std::vector<ChildEntry>& entries) override {
#ifndef _WIN32
    auto* inodeMetadataTable = mount_->getInodeMetadataTable();
#endif

    TreeInodeDebugInfo info;
    info.inodeNumber_ref() = ino.get();
    info.path_ref() = path.stringPiece().str();
    info.materialized_ref() = !hash.has_value();
    if (hash.has_value()) {
      info.treeHash_ref() =
          mount_->getObjectStore()->renderObjectId(hash.value());
    }
    info.refcount_ref() = fsRefcount;

    info.entries_ref()->reserve(entries.size());

    for (auto& entry : entries) {
      TreeInodeEntryDebugInfo entryInfo;
      entryInfo.name_ref() = entry.name.stringPiece().str();
      entryInfo.inodeNumber_ref() = entry.ino.get();

      // This could be enabled on Windows if InodeMetadataTable was removed.
#ifndef _WIN32
      if (auto metadata = (flags_ & eden_constants::DIS_COMPUTE_ACCURATE_MODE_)
              ? inodeMetadataTable->getOptional(entry.ino)
              : std::nullopt) {
        entryInfo.mode_ref() = metadata->mode;
      } else {
        entryInfo.mode_ref() = dtype_to_mode(entry.dtype);
      }
#else
      entryInfo.mode_ref() = dtype_to_mode(entry.dtype);
#endif

      entryInfo.loaded_ref() = entry.loadedChild != nullptr;
      entryInfo.materialized_ref() = !entry.hash.has_value();
      if (entry.hash.has_value()) {
        entryInfo.hash_ref() =
            mount_->getObjectStore()->renderObjectId(entry.hash.value());
      }

      if ((flags_ & eden_constants::DIS_COMPUTE_BLOB_SIZES_) &&
          dtype_t::Dir != entry.dtype) {
        if (entry.hash.has_value()) {
          // schedule fetching size from ObjectStore::getBlobSize
          requestedSizes_.push_back(RequestedSize{
              results_.size(), info.entries_ref()->size(), entry.hash.value()});
        } else {
#ifndef _WIN32
          entryInfo.fileSize_ref() =
              mount_->getOverlayFileAccess()->getFileSize(
                  entry.ino, entry.loadedChild.get());
#else
          // This following code ends up doing a stat in the working directory.
          // This is safe to do as Windows works very differently from
          // Linux/macOS when dealing with materialized files. In this code, we
          // know that the file is materialized because we do not have a hash
          // for it, and every materialized file is present on disk and
          // reading/stating it is guaranteed to be done without EdenFS
          // involvement. If somehow EdenFS is wrong, and this ends up
          // triggering a recursive call into EdenFS, we are detecting this and
          // simply bailing out very early in the callback.
          auto filePath = mount_->getPath() + path + entry.name;
          struct stat fileStat;
          if (::stat(filePath.c_str(), &fileStat) == 0) {
            entryInfo.fileSize_ref() = fileStat.st_size;
          } else {
            // Couldn't read the file, let's pretend it has a size of 0.
            entryInfo.fileSize_ref() = 0;
          }
#endif
        }
      }

      info.entries_ref()->push_back(entryInfo);
    }

    results_.push_back(std::move(info));
  }

  bool shouldRecurse(const ChildEntry& entry) override {
    if ((flags_ & eden_constants::DIS_REQUIRE_LOADED_) && !entry.loadedChild) {
      return false;
    }
    if ((flags_ & eden_constants::DIS_REQUIRE_MATERIALIZED_) &&
        entry.hash.has_value()) {
      return false;
    }
    return true;
  }

  void fillBlobSizes(ObjectFetchContext& fetchContext) {
    std::vector<ImmediateFuture<folly::Unit>> futures;
    futures.reserve(requestedSizes_.size());
    for (auto& request : requestedSizes_) {
      futures.push_back(mount_->getObjectStore()
                            ->getBlobSize(request.hash, fetchContext)
                            .thenValue([this, request](uint64_t blobSize) {
                              results_.at(request.resultIndex)
                                  .entries_ref()
                                  ->at(request.entryIndex)
                                  .fileSize_ref() = blobSize;
                            }));
    }
    collectAll(std::move(futures)).get();
  }

 private:
  struct RequestedSize {
    size_t resultIndex;
    size_t entryIndex;
    ObjectId hash;
  };

  EdenMount* mount_;
  int64_t flags_;
  std::vector<TreeInodeDebugInfo>& results_;
  std::vector<RequestedSize> requestedSizes_;
};

} // namespace

void EdenServiceHandler::debugInodeStatus(
    vector<TreeInodeDebugInfo>& inodeInfo,
    unique_ptr<string> mountPoint,
    unique_ptr<std::string> path,
    int64_t flags,
    std::unique_ptr<SyncBehavior> sync) {
  if (0 == flags) {
    flags = eden_constants::DIS_REQUIRE_LOADED_ |
        eden_constants::DIS_COMPUTE_BLOB_SIZES_;
  }

  auto syncTimeout = getSyncTimeout(*sync);
  auto helper = INSTRUMENT_THRIFT_CALL(
      DBG2, *mountPoint, *path, flags, syncTimeout.count());
  auto mountPath = AbsolutePathPiece{*mountPoint};
  auto edenMount = server_->getMount(mountPath);

  waitForPendingNotifications(*edenMount, syncTimeout)
      .thenValue([&](auto&&) {
        auto inode =
            inodeFromUserPath(*edenMount, *path, helper->getFetchContext())
                .asTreePtr();
        auto inodePath = inode->getPath().value();

        InodeStatusCallbacks callbacks{edenMount.get(), flags, inodeInfo};
        traverseObservedInodes(*inode, inodePath, callbacks);
        callbacks.fillBlobSizes(helper->getFetchContext());
      })
      .get();
}

void EdenServiceHandler::debugOutstandingFuseCalls(
    FOLLY_MAYBE_UNUSED std::vector<FuseCall>& outstandingCalls,
    FOLLY_MAYBE_UNUSED std::unique_ptr<std::string> mountPoint) {
#ifndef _WIN32
  auto helper = INSTRUMENT_THRIFT_CALL(DBG2);

  auto mountPath = AbsolutePathPiece{*mountPoint};
  auto edenMount = server_->getMount(mountPath);

  if (auto* fuseChannel = edenMount->getFuseChannel()) {
    for (const auto& call : fuseChannel->getOutstandingRequests()) {
      outstandingCalls.push_back(populateFuseCall(
          call.unique,
          call.request,
          *server_->getServerState()->getProcessNameCache()));
    }
  }
#else
  NOT_IMPLEMENTED();
#endif // !_WIN32
}

void EdenServiceHandler::debugOutstandingNfsCalls(
    FOLLY_MAYBE_UNUSED std::vector<NfsCall>& outstandingCalls,
    FOLLY_MAYBE_UNUSED std::unique_ptr<std::string> mountPoint) {
#ifndef _WIN32
  auto helper = INSTRUMENT_THRIFT_CALL(DBG2);

  auto mountPath = AbsolutePathPiece{*mountPoint};
  auto edenMount = server_->getMount(mountPath);

  if (auto* nfsdChannel = edenMount->getNfsdChannel()) {
    for (const auto& call : nfsdChannel->getOutstandingRequests()) {
      NfsCall nfsCall;
      nfsCall.xid_ref() = call.xid;
      outstandingCalls.push_back(nfsCall);
    }
  }
#else
  NOT_IMPLEMENTED();
#endif // !_WIN32
}

void EdenServiceHandler::debugOutstandingPrjfsCalls(
    FOLLY_MAYBE_UNUSED std::vector<PrjfsCall>& outstandingCalls,
    FOLLY_MAYBE_UNUSED std::unique_ptr<std::string> mountPoint) {
#ifdef _WIN32
  auto helper = INSTRUMENT_THRIFT_CALL(DBG2);

  auto mountPath = AbsolutePathPiece{*mountPoint};
  auto edenMount = server_->getMount(mountPath);

  if (auto* prjfsChannel = edenMount->getPrjfsChannel()) {
    for (const auto& call :
         prjfsChannel->getInner()->getOutstandingRequests()) {
      outstandingCalls.push_back(populatePrjfsCall(call.type, call.data));
    }
  }
#else
  NOT_IMPLEMENTED();
#endif // _WIN32
}

void EdenServiceHandler::debugStartRecordingActivity(
    ActivityRecorderResult& result,
    std::unique_ptr<std::string> mountPoint,
    std::unique_ptr<std::string> outputDir) {
  AbsolutePathPiece path;
  try {
    path = AbsolutePathPiece{*outputDir};
  } catch (const std::exception&) {
    throw newEdenError(
        EINVAL,
        EdenErrorType::ARGUMENT_ERROR,
        "path for output directory is invalid");
  }

  auto mount = server_->getMount(AbsolutePathPiece{*mountPoint});
  auto lockedPtr = mount->getActivityRecorder().wlock();
  // bool check on the wrapped pointer as lockedPtr is truthy as long
  // as we have the lock
  if (!lockedPtr->get()) {
    auto recorder = server_->makeActivityRecorder(mount);
    lockedPtr->swap(recorder);
  }
  uint64_t unique = lockedPtr->get()->addSubscriber(path);
  // unique_ref is signed but overflow is very unlikely because unique is UNIX
  // timestamp in seconds.
  result.unique_ref() = unique;
}

void EdenServiceHandler::debugStopRecordingActivity(
    ActivityRecorderResult& result,
    std::unique_ptr<std::string> mountPoint,
    int64_t unique) {
  auto lockedPtr = server_->getMount(AbsolutePathPiece{*mountPoint})
                       ->getActivityRecorder()
                       .wlock();
  auto* activityRecorder = lockedPtr->get();
  if (!activityRecorder) {
    return;
  }

  auto outputPath = activityRecorder->removeSubscriber(unique);
  if (outputPath.has_value()) {
    result.unique_ref() = unique;
    result.path_ref() = outputPath.value();
  }

  if (activityRecorder->getSubscribers().size() == 0) {
    lockedPtr->reset();
  }
}

void EdenServiceHandler::debugListActivityRecordings(
    ListActivityRecordingsResult& result,
    std::unique_ptr<std::string> mountPoint) {
  auto mount = server_->getMount(AbsolutePathPiece{*mountPoint});
  auto lockedPtr = mount->getActivityRecorder().rlock();
  auto* activityRecorder = lockedPtr->get();
  if (!activityRecorder) {
    return;
  }

  std::vector<ActivityRecorderResult> recordings;
  auto subscribers = activityRecorder->getSubscribers();
  recordings.reserve(subscribers.size());
  for (auto const& subscriber : subscribers) {
    ActivityRecorderResult recording;
    recording.unique_ref() = std::get<0>(subscriber);
    recording.path_ref() = std::get<1>(subscriber);
    recordings.push_back(std::move(recording));
  }
  result.recordings_ref() = recordings;
}

void EdenServiceHandler::debugGetInodePath(
    InodePathDebugInfo& info,
    std::unique_ptr<std::string> mountPoint,
    int64_t inodeNumber) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3);
  auto inodeNum = static_cast<InodeNumber>(inodeNumber);
  auto mountPath = AbsolutePathPiece{*mountPoint};
  auto inodeMap = server_->getMount(mountPath)->getInodeMap();

  auto relativePath = inodeMap->getPathForInode(inodeNum);
  // Check if the inode is loaded
  info.loaded_ref() = inodeMap->lookupLoadedInode(inodeNum) != nullptr;
  // If getPathForInode returned none then the inode is unlinked
  info.linked_ref() = relativePath != std::nullopt;
  info.path_ref() = relativePath ? relativePath->stringPiece().str() : "";
}

void EdenServiceHandler::clearFetchCounts() {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3);

  for (auto& mount : server_->getMountPoints()) {
    mount->getObjectStore()->clearFetchCounts();
  }
}

void EdenServiceHandler::clearFetchCountsByMount(
    std::unique_ptr<std::string> mountPoint) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3);
  auto mountPath = AbsolutePathPiece{*mountPoint};
  auto mount = server_->getMount(mountPath);
  mount->getObjectStore()->clearFetchCounts();
}

void EdenServiceHandler::startRecordingBackingStoreFetch() {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3);
  for (auto& backingStore : server_->getBackingStores()) {
    backingStore->startRecordingFetch();
  }
}

void EdenServiceHandler::stopRecordingBackingStoreFetch(
    GetFetchedFilesResult& results) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3);
  for (const auto& backingStore : server_->getBackingStores()) {
    auto filePaths = backingStore->stopRecordingFetch();
    // recording is only implemented for HgQueuedBackingStore at the moment
    // TODO: remove these dynamic casts in favor of a QueryInterface method
    // BackingStore -> LocalStoreCachedBackingStore
    std::shared_ptr<HgQueuedBackingStore> hgBackingStore{nullptr};
    auto localStoreCachedBackingStore =
        std::dynamic_pointer_cast<LocalStoreCachedBackingStore>(backingStore);
    if (!localStoreCachedBackingStore) {
      // BackingStore -> HgQueuedBackingStore
      hgBackingStore =
          std::dynamic_pointer_cast<HgQueuedBackingStore>(backingStore);
    } else {
      // LocalStoreCachedBackingStore -> HgQueuedBackingStore
      hgBackingStore = std::dynamic_pointer_cast<HgQueuedBackingStore>(
          localStoreCachedBackingStore->getBackingStore());
    }
    if (hgBackingStore) {
      (*results.fetchedFilePaths_ref())["HgQueuedBackingStore"].insert(
          filePaths.begin(), filePaths.end());
    }
  }
} // namespace eden

void EdenServiceHandler::getAccessCounts(
    GetAccessCountsResult& result,
    int64_t duration) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3);

  result.cmdsByPid_ref() =
      server_->getServerState()->getProcessNameCache()->getAllProcessNames();

  auto seconds = std::chrono::seconds{duration};

  for (auto& mount : server_->getMountPoints()) {
    auto& mountStr = mount->getPath().value();
    auto& pal = mount->getProcessAccessLog();

    auto& pidFetches = mount->getObjectStore()->getPidFetches();

    MountAccesses& ma = result.accessesByMount_ref()[mountStr];
    for (auto& [pid, accessCounts] : pal.getAccessCounts(seconds)) {
      ma.accessCountsByPid_ref()[pid] = accessCounts;
    }

    for (auto& [pid, fetchCount] : *pidFetches.rlock()) {
      ma.fetchCountsByPid_ref()[pid] = fetchCount;
    }
  }
}

void EdenServiceHandler::clearAndCompactLocalStore() {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG1);
  server_->getLocalStore()->clearCachesAndCompactAll();
}

void EdenServiceHandler::debugClearLocalStoreCaches() {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG1);
  server_->getLocalStore()->clearCaches();
}

void EdenServiceHandler::debugCompactLocalStorage() {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG1);
  server_->getLocalStore()->compactStorage();
}

// TODO(T119221752): add more BackingStore subclasses to this command. We
// currently only support HgQueuedBackingStores
int64_t EdenServiceHandler::debugDropAllPendingRequests() {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG1);
  auto stores = server_->getHgQueuedBackingStores();
  int64_t numDropped = 0;
  for (auto& store : stores) {
    numDropped += store->dropAllPendingRequestsFromQueue();
  }
  return numDropped;
}

int64_t EdenServiceHandler::unloadInodeForPath(
    FOLLY_MAYBE_UNUSED unique_ptr<string> mountPoint,
    FOLLY_MAYBE_UNUSED std::unique_ptr<std::string> path,
    FOLLY_MAYBE_UNUSED std::unique_ptr<TimeSpec> age) {
#ifndef _WIN32
  auto helper = INSTRUMENT_THRIFT_CALL(DBG1, *mountPoint, *path);
  auto mountPath = AbsolutePathPiece{*mountPoint};
  auto edenMount = server_->getMount(mountPath);

  TreeInodePtr inode =
      inodeFromUserPath(*edenMount, *path, helper->getFetchContext())
          .asTreePtr();
  auto cutoff = std::chrono::system_clock::now() -
      std::chrono::seconds(*age->seconds_ref()) -
      std::chrono::nanoseconds(*age->nanoSeconds_ref());
  auto cutoff_ts = folly::to<timespec>(cutoff);
  return inode->unloadChildrenLastAccessedBefore(cutoff_ts);
#else
  NOT_IMPLEMENTED();
#endif
}

void EdenServiceHandler::getStatInfo(
    InternalStats& result,
    std::unique_ptr<GetStatInfoParams> params) {
  int64_t statsMask = params->get_statsMask();
  // return all stats when mask not provided
  // TODO: remove when no old clients exists
  if (0 == statsMask) {
    statsMask = ~0;
  }

  auto helper = INSTRUMENT_THRIFT_CALL(DBG3);

  if (statsMask & eden_constants::STATS_MOUNTS_STATS_) {
    auto mountList = server_->getMountPoints();
    std::map<PathString, MountInodeInfo> mountPointInfo = {};
    std::map<PathString, JournalInfo> mountPointJournalInfo = {};
    for (auto& mount : mountList) {
      auto inodeMap = mount->getInodeMap();
      // Set LoadedInde Count and unloaded Inode count for the mountPoint.
      MountInodeInfo mountInodeInfo;
      auto counts = inodeMap->getInodeCounts();
      mountInodeInfo.unloadedInodeCount_ref() = counts.unloadedInodeCount;
      mountInodeInfo.loadedFileCount_ref() = counts.fileCount;
      mountInodeInfo.loadedTreeCount_ref() = counts.treeCount;

      JournalInfo journalThrift;
      if (auto journalStats = mount->getJournal().getStats()) {
        journalThrift.entryCount_ref() = journalStats->entryCount;
        journalThrift.durationSeconds_ref() =
            journalStats->getDurationInSeconds();
      } else {
        journalThrift.entryCount_ref() = 0;
        journalThrift.durationSeconds_ref() = 0;
      }
      journalThrift.memoryUsage_ref() =
          mount->getJournal().estimateMemoryUsage();
      mountPointJournalInfo[mount->getPath().stringPiece().str()] =
          journalThrift;

      mountPointInfo[mount->getPath().stringPiece().str()] = mountInodeInfo;
    }
    result.mountPointInfo_ref() = mountPointInfo;
    result.mountPointJournalInfo_ref() = mountPointJournalInfo;
  }

  if (statsMask & eden_constants::STATS_COUNTERS_) {
    // Get the counters and set number of inodes unloaded by periodic unload
    // job.
    auto counters = fb303::ServiceData::get()->getCounters();
    result.counters_ref() = counters;
    size_t periodicUnloadCount{0};
    for (auto& mount : server_->getMountPoints()) {
      periodicUnloadCount +=
          counters[mount->getCounterName(CounterName::PERIODIC_INODE_UNLOAD)];
    }

    result.periodicUnloadCount_ref() = periodicUnloadCount;
  }

  if (statsMask & eden_constants::STATS_PRIVATE_BYTES_) {
    auto privateDirtyBytes = facebook::eden::proc_util::calculatePrivateBytes();
    if (privateDirtyBytes) {
      result.privateBytes_ref() = privateDirtyBytes.value();
    }
  }

  if (statsMask & eden_constants::STATS_RSS_BYTES_) {
    auto memoryStats = facebook::eden::proc_util::readMemoryStats();
    if (memoryStats) {
      result.vmRSSBytes_ref() = memoryStats->resident;
    }
  }

  if (statsMask & eden_constants::STATS_SMAPS_) {
    // Note: this will be removed in a subsequent commit.
    // We now report periodically via ServiceData
    std::string smaps;
    if (folly::readFile("/proc/self/smaps", smaps)) {
      result.smaps_ref() = std::move(smaps);
    }
  }

  if (statsMask & eden_constants::STATS_CACHE_STATS_) {
    const auto blobCacheStats = server_->getBlobCache()->getStats();
    result.blobCacheStats_ref() = CacheStats{};
    result.blobCacheStats_ref()->entryCount_ref() = blobCacheStats.objectCount;
    result.blobCacheStats_ref()->totalSizeInBytes_ref() =
        blobCacheStats.totalSizeInBytes;
    result.blobCacheStats_ref()->hitCount_ref() = blobCacheStats.hitCount;
    result.blobCacheStats_ref()->missCount_ref() = blobCacheStats.missCount;
    result.blobCacheStats_ref()->evictionCount_ref() =
        blobCacheStats.evictionCount;
    result.blobCacheStats_ref()->dropCount_ref() = blobCacheStats.dropCount;

    const auto treeCacheStats = server_->getTreeCache()->getStats();
    result.treeCacheStats_ref() = CacheStats{};
    result.treeCacheStats_ref()->entryCount_ref() = treeCacheStats.objectCount;
    result.treeCacheStats_ref()->totalSizeInBytes_ref() =
        treeCacheStats.totalSizeInBytes;
    result.treeCacheStats_ref()->hitCount_ref() = treeCacheStats.hitCount;
    result.treeCacheStats_ref()->missCount_ref() = treeCacheStats.missCount;
    result.treeCacheStats_ref()->evictionCount_ref() =
        treeCacheStats.evictionCount;
  }
}

void EdenServiceHandler::flushStatsNow() {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG3);
  server_->flushStatsNow();
}

folly::SemiFuture<Unit>
EdenServiceHandler::semifuture_invalidateKernelInodeCache(
    FOLLY_MAYBE_UNUSED std::unique_ptr<std::string> mountPoint,
    FOLLY_MAYBE_UNUSED std::unique_ptr<std::string> path) {
#ifndef _WIN32
  auto helper = INSTRUMENT_THRIFT_CALL(DBG2, *mountPoint, *path);
  auto mountPath = AbsolutePathPiece{*mountPoint};
  auto edenMount = server_->getMount(mountPath);
  InodePtr inode =
      inodeFromUserPath(*edenMount, *path, helper->getFetchContext());

  if (auto* fuseChannel = edenMount->getFuseChannel()) {
    // Invalidate cached pages and attributes
    fuseChannel->invalidateInode(inode->getNodeId(), 0, 0);

    const auto treePtr = inode.asTreePtrOrNull();

    // Invalidate all parent/child relationships potentially cached.
    if (treePtr != nullptr) {
      const auto& dir = treePtr->getContents().rlock();
      for (const auto& entry : dir->entries) {
        fuseChannel->invalidateEntry(inode->getNodeId(), entry.first);
      }
    }

    // Wait for all of the invalidations to complete
    return fuseChannel->flushInvalidations();
  }

  if (auto* nfsChannel = edenMount->getNfsdChannel()) {
    inode->forceMetadataUpdate();
    auto& fetchContext = helper->getFetchContext();
    auto rawInodePtr = inode.get();
    return wrapImmediateFuture(
               std::move(helper),
               rawInodePtr->stat(fetchContext)
                   .thenValue(
                       [nfsChannel,
                        canonicalMountPoint = canonicalPath(*mountPoint),
                        inode = std::move(inode),
                        path = std::move(path),
                        edenMount = std::move(edenMount),
                        &fetchContext =
                            fetchContext](struct stat&& stat) mutable
                       -> ImmediateFuture<folly::Unit> {
                         nfsChannel->invalidate(
                             canonicalMountPoint + RelativePath{*path},
                             stat.st_mode);
                         const auto treePtr = inode.asTreePtrOrNull();
                         // Invalidate all children as well. There isn't really
                         // a way to invalidate the entry cache for nfs so we
                         // settle for invalidating the children themselves.
                         if (treePtr != nullptr) {
                           const auto& dir = treePtr->getContents().rlock();
                           std::vector<ImmediateFuture<folly::Unit>>
                               childInvalidations{};
                           for (const auto& entry : dir->entries) {
                             auto childPath = RelativePath{*path} + entry.first;
                             auto childInode = inodeFromUserPath(
                                 *edenMount,
                                 childPath.stringPiece().str(),
                                 fetchContext);
                             childInode->forceMetadataUpdate();
                             childInvalidations.push_back(
                                 childInode->stat(fetchContext)
                                     .thenValue(
                                         [nfsChannel,
                                          canonicalMountPoint,
                                          childPath](struct stat&& stat) {
                                           nfsChannel->invalidate(
                                               canonicalMountPoint + childPath,
                                               stat.st_mode);
                                           return folly::Unit();
                                         }));
                           }
                           return collectAll(std::move(childInvalidations))
                               .unit();
                         }
                         return folly::unit;
                       })
                   .ensure([nfsChannel]() {
                     return nfsChannel->flushInvalidations();
                   }))
        .semi();
  }

  return EDEN_BUG_FUTURE(folly::Unit) << "Unsupported Channel type.";
#else
  NOT_IMPLEMENTED();
#endif // !_WIN32
}

void EdenServiceHandler::enableTracing() {
  XLOG(INFO) << "Enabling tracing";
  eden::enableTracing();
}
void EdenServiceHandler::disableTracing() {
  XLOG(INFO) << "Disabling tracing";
  eden::disableTracing();
}

void EdenServiceHandler::getTracePoints(std::vector<TracePoint>& result) {
  auto compactTracePoints = getAllTracepoints();
  for (auto& point : compactTracePoints) {
    TracePoint tp;
    tp.timestamp_ref() = point.timestamp.count();
    tp.traceId_ref() = point.traceId;
    tp.blockId_ref() = point.blockId;
    tp.parentBlockId_ref() = point.parentBlockId;
    if (point.name) {
      tp.name_ref() = std::string(point.name);
    }
    if (point.start) {
      tp.event_ref() = TracePointEvent::START;
    } else if (point.stop) {
      tp.event_ref() = TracePointEvent::STOP;
    }
    result.emplace_back(std::move(tp));
  }
}

namespace {
std::optional<folly::exception_wrapper> getFaultError(
    apache::thrift::optional_field_ref<std::string&> errorType,
    apache::thrift::optional_field_ref<std::string&> errorMessage) {
  if (!errorType.has_value() && !errorMessage.has_value()) {
    return std::nullopt;
  }

  auto createException =
      [](StringPiece type, const std::string& msg) -> folly::exception_wrapper {
    if (type == "runtime_error") {
      return std::runtime_error(msg);
    } else if (type.startsWith("errno:")) {
      auto errnum = folly::to<int>(type.subpiece(6));
      return std::system_error(errnum, std::generic_category(), msg);
    }
    // If we want to support other error types in the future they should
    // be added here.
    throw newEdenError(
        EdenErrorType::GENERIC_ERROR, "unknown error type ", type);
  };

  return createException(
      errorType.value_or("runtime_error"),
      errorMessage.value_or("injected error"));
}
} // namespace

void EdenServiceHandler::injectFault(unique_ptr<FaultDefinition> fault) {
  auto& injector = server_->getServerState()->getFaultInjector();
  if (*fault->block_ref()) {
    injector.injectBlock(
        *fault->keyClass_ref(),
        *fault->keyValueRegex_ref(),
        *fault->count_ref());
    return;
  }

  auto error = getFaultError(fault->errorType_ref(), fault->errorMessage_ref());
  std::chrono::milliseconds delay(*fault->delayMilliseconds_ref());
  if (error.has_value()) {
    if (delay.count() > 0) {
      injector.injectDelayedError(
          *fault->keyClass_ref(),
          *fault->keyValueRegex_ref(),
          delay,
          error.value(),
          *fault->count_ref());
    } else {
      injector.injectError(
          *fault->keyClass_ref(),
          *fault->keyValueRegex_ref(),
          error.value(),
          *fault->count_ref());
    }
  } else {
    if (delay.count() > 0) {
      injector.injectDelay(
          *fault->keyClass_ref(),
          *fault->keyValueRegex_ref(),
          delay,
          *fault->count_ref());
    } else {
      injector.injectNoop(
          *fault->keyClass_ref(),
          *fault->keyValueRegex_ref(),
          *fault->count_ref());
    }
  }
}

bool EdenServiceHandler::removeFault(unique_ptr<RemoveFaultArg> fault) {
  auto& injector = server_->getServerState()->getFaultInjector();
  return injector.removeFault(
      *fault->keyClass_ref(), *fault->keyValueRegex_ref());
}

int64_t EdenServiceHandler::unblockFault(unique_ptr<UnblockFaultArg> info) {
  auto& injector = server_->getServerState()->getFaultInjector();
  auto error = getFaultError(info->errorType_ref(), info->errorMessage_ref());

  if (!info->keyClass_ref().has_value()) {
    if (info->keyValueRegex_ref().has_value()) {
      throw newEdenError(
          EINVAL,
          EdenErrorType::ARGUMENT_ERROR,
          "cannot specify a key value regex without a key class");
    }
    if (error.has_value()) {
      return injector.unblockAllWithError(error.value());
    } else {
      return injector.unblockAll();
    }
  }

  const auto& keyClass = info->keyClass_ref().value();
  std::string keyValueRegex = info->keyValueRegex_ref().value_or(".*");
  if (error.has_value()) {
    return injector.unblockWithError(keyClass, keyValueRegex, error.value());
  } else {
    return injector.unblock(keyClass, keyValueRegex);
  }
}

void EdenServiceHandler::reloadConfig() {
  auto helper = INSTRUMENT_THRIFT_CALL(INFO);
  server_->reloadConfig();
}

void EdenServiceHandler::getDaemonInfo(DaemonInfo& result) {
  auto helper = INSTRUMENT_THRIFT_CALL(DBG4);

  fb303::cpp2::fb303_status status = [&] {
    switch (server_->getStatus()) {
      case EdenServer::RunState::STARTING:
        return facebook::fb303::cpp2::fb303_status::STARTING;
      case EdenServer::RunState::RUNNING:
        return facebook::fb303::cpp2::fb303_status::ALIVE;
      case EdenServer::RunState::SHUTTING_DOWN:
        return facebook::fb303::cpp2::fb303_status::STOPPING;
    }
    EDEN_BUG() << "unexpected EdenServer status " << enumValue(status);
  }();

  result.pid_ref() = getpid();
  result.commandLine_ref() = originalCommandLine_;
  result.status_ref() = status;

  auto now = std::chrono::steady_clock::now();
  std::chrono::duration<float> uptime = now - server_->getStartTime();
  result.uptime_ref() = uptime.count();
}

void EdenServiceHandler::checkPrivHelper(PrivHelperInfo& result) {
#ifndef _WIN32
  auto privhelper = server_->getServerState()->getPrivHelper();
  result.connected_ref() = privhelper->checkConnection();
#else
  result.connected_ref() = true;
#endif
}

int64_t EdenServiceHandler::getPid() {
  return getpid();
}

void EdenServiceHandler::initiateShutdown(std::unique_ptr<std::string> reason) {
  auto helper = INSTRUMENT_THRIFT_CALL(INFO);
  XLOG(INFO) << "initiateShutdown requested, reason: " << *reason;
  server_->stop();
}

void EdenServiceHandler::getConfig(
    EdenConfigData& result,
    unique_ptr<GetConfigParams> params) {
  auto state = server_->getServerState();
  auto config = state->getEdenConfig(*params->reload_ref());

  result = config->toThriftConfigData();
}

std::optional<pid_t> EdenServiceHandler::getAndRegisterClientPid() {
#ifndef _WIN32
  // The Cpp2RequestContext for a thrift request is kept in a thread local
  // on the thread which the request originates. This means this must be run
  // on the Thread in which a thrift request originates.
  auto connectionContext = getRequestContext();
  // if connectionContext will be a null pointer in an async method, so we
  // need to check for this
  if (connectionContext) {
    pid_t clientPid =
        connectionContext->getConnectionContext()->getPeerEffectiveCreds()->pid;
    server_->getServerState()->getProcessNameCache()->add(clientPid);
    return clientPid;
  }
  return std::nullopt;
#else
  return std::nullopt;
#endif
}

} // namespace facebook::eden
