/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once
#include <folly/File.h>
#include <folly/Portability.h>
#include <folly/Synchronized.h>
#include <optional>
#include "eden/fs/fuse/Invalidation.h"
#include "eden/fs/inodes/CheckoutAction.h"
#include "eden/fs/inodes/DirEntry.h"
#include "eden/fs/inodes/InodeBase.h"
#include "eden/fs/inodes/InodeOrTreeOrEntry.h"
#include "eden/fs/utils/PathFuncs.h"

namespace facebook {
namespace eden {

class CheckoutAction;
class CheckoutContext;
class DiffContext;
class FuseDirList;
class NfsDirList;
class EdenMount;
class GitIgnoreStack;
class DiffCallback;
class InodeMap;
class ObjectFetchContext;
class ObjectStore;
class Overlay;
class RenameLock;
class Tree;
class TreeEntry;
class TreeInodeDebugInfo;
class PrjfsDirEntry;

constexpr folly::StringPiece kDotEdenName{".eden"};

/**
 * The state of a TreeInode as held in memory.
 */
struct TreeInodeState {
  explicit TreeInodeState(DirContents&& dir, std::optional<ObjectId> hash)
      : entries{std::forward<DirContents>(dir)}, treeHash{hash} {}

  bool isMaterialized() const {
    return !treeHash.has_value();
  }
  void setMaterialized() {
    treeHash = std::nullopt;
  }

  DirContents entries;

  /**
   * If this TreeInode is unmaterialized (identical to an existing source
   * control Tree), treeHash contains the ID of the source control Tree
   * that this TreeInode is identical to.
   *
   * If this TreeInode is materialized (possibly modified from source
   * control, and backed by the Overlay instead of a source control Tree),
   * treeHash will be none.
   */
  std::optional<ObjectId> treeHash;
};

/**
 * Represents a directory in the file system.
 */
class TreeInode final : public InodeBaseMetadata<DirContents> {
 public:
  using Base = InodeBaseMetadata<DirContents>;

  enum : int { WRONG_TYPE_ERRNO = ENOTDIR };

  /**
   * Construct a TreeInode from a source control tree.
   */
  TreeInode(
      InodeNumber ino,
      TreeInodePtr parent,
      PathComponentPiece name,
      mode_t initialMode,
      std::shared_ptr<const Tree>&& tree);

  /**
   * Construct an inode that only has backing in the Overlay area.
   */
  TreeInode(
      InodeNumber ino,
      TreeInodePtr parent,
      PathComponentPiece name,
      mode_t initialMode,
      const std::optional<InodeTimestamps>& initialTimestamps,
      DirContents&& dir,
      std::optional<ObjectId> treeHash);

  /**
   * Construct the root TreeInode from a source control commit's root.
   */
  TreeInode(EdenMount* mount, std::shared_ptr<const Tree>&& tree);

  /**
   * Construct the root inode from data saved in the overlay.
   */
  TreeInode(
      EdenMount* mount,
      DirContents&& dir,
      std::optional<ObjectId> treeHash);

  ~TreeInode() override;

  ImmediateFuture<struct stat> stat(ObjectFetchContext& context) override;

#ifndef _WIN32
  folly::Future<struct stat> setattr(
      const DesiredMetadata& desired,
      ObjectFetchContext& fetchContext) override;

  ImmediateFuture<std::vector<std::string>> listxattr() override;
  ImmediateFuture<std::string> getxattr(
      folly::StringPiece name,
      ObjectFetchContext& context) override;
#endif // !_WIN32

  /**
   * Get the inode object for a child of this directory.
   *
   * Implements getOrLoadChild if loadInodes is true. If loadInodes is false and
   * the Inode load is already-in-progress, this may NOT return the loading
   * inode. Otherwise, the returned InodeOrTreeOrEntry may contain a ObjectStore
   * Tree or a DirEntry/TreeEntry representing the entry.
   */
  ImmediateFuture<InodeOrTreeOrEntry> getOrFindChild(
      PathComponentPiece name,
      ObjectFetchContext& context,
      bool loadInodes);

  /**
   * Get the inode object for a child of this directory.
   *
   * The Inode object will be loaded if it is not already loaded.
   */
  ImmediateFuture<InodePtr> getOrLoadChild(
      PathComponentPiece name,
      ObjectFetchContext& context);
  ImmediateFuture<TreeInodePtr> getOrLoadChildTree(
      PathComponentPiece name,
      ObjectFetchContext& context);

  /**
   * Recursively look up a child inode.
   *
   * The Inode object in question, and all intervening TreeInode objects,
   * will be loaded if they are not already loaded.
   */
  ImmediateFuture<InodePtr> getChildRecursive(
      RelativePathPiece name,
      ObjectFetchContext& context);

  InodeNumber getChildInodeNumber(PathComponentPiece name);

  FOLLY_NODISCARD ImmediateFuture<folly::Unit> rename(
      PathComponentPiece name,
      TreeInodePtr newParent,
      PathComponentPiece newName,
      InvalidationRequired invalidate,
      ObjectFetchContext& context);

#ifndef _WIN32
  FuseDirList
  fuseReaddir(FuseDirList&& list, off_t off, ObjectFetchContext& context);

  /**
   * Populate the list with as many directory entries as possible starting from
   * the inode start.
   *
   * Return the filled directory list as well as a boolean indicating if the
   * listing is complete.
   */
  std::tuple<NfsDirList, bool>
  nfsReaddir(NfsDirList&& list, off_t off, ObjectFetchContext& context);
#endif

  const folly::Synchronized<TreeInodeState>& getContents() const {
    return contents_;
  }
  folly::Synchronized<TreeInodeState>& getContents() {
    return contents_;
  }

  FileInodePtr symlink(
      PathComponentPiece name,
      folly::StringPiece contents,
      InvalidationRequired invalidate);

  TreeInodePtr
  mkdir(PathComponentPiece name, mode_t mode, InvalidationRequired invalidate);
  FOLLY_NODISCARD ImmediateFuture<folly::Unit> unlink(
      PathComponentPiece name,
      InvalidationRequired invalidate,
      ObjectFetchContext& context);
  FOLLY_NODISCARD ImmediateFuture<folly::Unit> rmdir(
      PathComponentPiece name,
      InvalidationRequired invalidate,
      ObjectFetchContext& context);

  /**
   * Remove the file or directory starting at name.
   *
   * In the case where name is a directory, this does a recursive removal of
   * all of its children too. This method ensures the invalidation is flushed
   * so the caller would see the up-to-date state after when call is finished.
   *
   * Note that this may fail if a concurrent file/directory creation is being
   * performed in that hierarchy. The caller is responsible for handling this
   * and potentially calling this function again.
   */
  ImmediateFuture<folly::Unit> removeRecursively(
      PathComponentPiece name,
      InvalidationRequired invalidate,
      ObjectFetchContext& context);

  /**
   * Attempts to remove and unlink children of this inode. Under concurrent
   * modification, it is not guaranteed that TreeInode is empty after this
   * function returns.
   */
  void removeAllChildrenRecursively(
      InvalidationRequired invalidate,
      ObjectFetchContext& context,
      const RenameLock& renameLock);

  /**
   * For unloaded nodes, the removal should be simpler: remove the node
   * from entries and update the overlay.
   * If the return value is valid, the entry was not removed, and the child's
   * loaded inode was returned.
   */
  InodePtr tryRemoveUnloadedChild(
      PathComponentPiece name,
      InvalidationRequired invalidate);

  /**
   * Create a filesystem node.
   * Only unix domain sockets and regular files are supported; attempting to
   * create any other kind of node will fail.
   */
  FileInodePtr mknod(
      PathComponentPiece name,
      mode_t mode,
      dev_t rdev,
      InvalidationRequired invalidate);

  /**
   * Compute differences between a source control Tree and the current inode
   * state.
   *
   * @param context A pointer to the DiffContext containing parameters for the
   *     current diff operation.  The caller is responsible for ensuring that
   *     the DiffContext object remains valid until this diff completes.
   * @param currentPath The path to this Tree, as used for the purpose of diff
   *     computation.  Note that we do not block renames and other filesystem
   *     layout changes during diff operations, so this might not actually
   *     correspond to the current TreeInode's path.  However, it was the path
   *     that we used for computing ignored status, so we want to report diff
   *     results using this path.  Even if it may not currently be the
   *     TreeInode's path it reflects the path that used to be correct at some
   *     point since the diff started.
   * @param tree The source control Tree to compare the current state against.
   *     This may be null when comparing a portion of the file system tree that
   *     does not exist in source control.
   * @param parentIgnore A GitIgnoreStack containing the gitignore data for all
   *     parent directories of this one.  This parameter may be null if
   *     isIgnored is true.  The caller must ensure that this GitIgnoreStack
   *     object remains valid until the returned Future object completes.
   * @param isIgnored  Whether or not the current directory is ignored
   *     according to source control ignore rules.
   *
   * @return Returns a Future that will be fulfilled when the diff operation
   *     completes.  The caller must ensure that the DiffCallback parameter
   *     remains valid until this Future completes.
   */
  folly::Future<folly::Unit> diff(
      DiffContext* context,
      RelativePathPiece currentPath,
      std::shared_ptr<const Tree> tree,
      const GitIgnoreStack* parentIgnore,
      bool isIgnored);

  /**
   * Update this directory so that it matches the specified source control Tree
   * object.
   *
   * @param ctx The CheckoutContext for the current checkout operation.
   *     The caller guarantees that the CheckoutContext argument will remain
   *     valid until the returned Future completes.
   * @param fromTree The Tree object that the checkout operation is moving
   *     from.  This argument is necessary to detect conflicts between the
   *     current inode state and the expected previous source control state.
   *     This argument may be null when updating a TreeInode that did not exist
   *     in source control in the previous commit state.
   * @param toTree The Tree object that the checkout operation is moving to.
   *     This argument may be null if this path no longer exists in the
   *     destination commit.  This tree inode will not be unlinked even if
   *     toTree is null. The caller is responsible for unlinking if necessary.
   *
   * @return Returns a future that will be fulfilled once this tree and all of
   *     its children have been updated.
   */
  FOLLY_NODISCARD folly::Future<folly::Unit> checkout(
      CheckoutContext* ctx,
      std::shared_ptr<const Tree> fromTree,
      std::shared_ptr<const Tree> toTree);

  /**
   * Update this directory when a child entry is materialized.
   *
   * This will materialize this directory if it is not already materialized,
   * and will record that the child in question is materialized.
   *
   * This method should only be called by the child inode in question.
   *
   * With regards to specific implementation details of this API:
   * - The child inode must not be holding locks on itself when calling this
   *   method.  Typically the child updates its own in-memory state first, then
   *   releases its lock before calling childMaterialized() on its parent.
   * - The child should have written out its overlay data on disk before
   *   calling this method.  This ensures that the child always has overlay
   *   data on disk whenever its parent directory's overlay data indicates that
   *   the child is materialized.
   */
  void childMaterialized(
      const RenameLock& renameLock,
      PathComponentPiece childName);

  /**
   * Update this directory when a child entry is dematerialized.
   *
   * This method should only be called by the child inode in question.
   *
   * With regards to specific implementation details of this API:
   * - The child inode must not be holding locks on itself when calling this
   *   method.  Typically the child updates its own in-memory state first, then
   *   releases its lock before calling childMaterialized() on its parent.
   * - The child should delay removing its on-disk overlay state until after
   *   this method returns.  This ensures that the child always has overlay
   *   data on disk whenever its parent directory's overlay data indicates that
   *   the child is materialized.
   */
  void childDematerialized(
      const RenameLock& renameLock,
      PathComponentPiece childName,
      ObjectId childScmHash);

  /**
   * Internal API only for use by InodeMap.
   *
   * InodeMap will call this API when a child inode needs to be loaded.
   * The TreeInode will call InodeMap::inodeLoadComplete() or
   * InodeMap::inodeLoadFailed() when the load finishes.
   */
  void loadChildInode(PathComponentPiece name, InodeNumber number);

  /**
   * Internal API only for use by InodeMap.
   *
   * InodeMap will this API when a child inode that has been unlinked
   * needs to be loaded.
   *
   * The TreeInode will call InodeMap::inodeLoadComplete() or
   * InodeMap::inodeLoadFailed() when the load finishes.
   */
  void loadUnlinkedChildInode(
      PathComponentPiece name,
      InodeNumber number,
      std::optional<ObjectId> hash,
      mode_t mode);

  /**
   * Unload all unreferenced children under this tree (recursively).
   *
   * This walks the children underneath this tree, unloading any inodes that
   * are unreferenced by Eden. If an inode is unreferenced by Eden but
   * still has a positive FUSE reference count, it will be unloaded and moved
   * into the InodeMap's unloadedInodes map.
   *
   * Returns the number of inodes unloaded.
   */
  size_t unloadChildrenNow();

  /**
   * Unload all children, recursively, neither referenced internally by Eden nor
   * by FUSE or ProjectedFS.
   *
   * Returns the number of inodes unloaded.
   */
  size_t unloadChildrenUnreferencedByFs();

#ifndef _WIN32
  /**
   * Unload all unreferenced inodes under this tree whose last access time is
   * older than the specified cutoff.
   *
   * Returns the number of inodes unloaded.
   */
  size_t unloadChildrenLastAccessedBefore(const timespec& cutoff);
#endif

  /*
   * Update a tree entry as part of a checkout operation.
   *
   * Returns whether or not the tree's contents were updated and the inode's
   * readdir cache must be flushed.
   *
   * This helper function is only to be used by CheckoutAction.
   *
   * @param ctx The CheckoutContext for the current checkout operation.
   *     The caller guarantees that the CheckoutContext argument will remain
   *     valid until the returned Future completes.
   * @param name The name of the child entry being replaced.
   * @param inode A pointer to the child InodeBase that is being updated.
   *     The path to this inode is guaranteed to match the name parameter.
   * @param oldTree If this entry referred to Tree in the source commit,
   *     then oldTree will be a pointer to its source control state.  oldTree
   *     will be null if this entry did not exist or if it referred to a Blob
   *     in the source commit.
   * @param newTree If this entry refers to Tree in the destination commit,
   *     then newTree will be a pointer to its source control state.  newTree
   *     will be null if this entry does not exist or if it refers to a Blob in
   *     the source commit.
   * @param newScmEntry The desired source control state for the new entry,
   *     or std::nullopt if the entry does not exist in the destination commit.
   *     This entry will refer to a tree if and only if the newTree parameter
   *     is non-null.
   */
  FOLLY_NODISCARD folly::Future<InvalidationRequired> checkoutUpdateEntry(
      CheckoutContext* ctx,
      PathComponentPiece name,
      InodePtr inode,
      std::shared_ptr<const Tree> oldTree,
      std::shared_ptr<const Tree> newTree,
      const std::optional<TreeEntry>& newScmEntry);

  /**
   * Returns a copy of this inode's metadata.
   */
#ifndef _WIN32
  InodeMetadata getMetadata() const override;
#endif

  void forceMetadataUpdate() override;

 private:
  class TreeRenameLocks;
  class IncompleteInodeLoad;

#ifndef _WIN32
  InodeMetadata getMetadataLocked(const DirContents&) const;
#endif

  /**
   * The InodeMap is guaranteed to remain valid for at least the lifetime of
   * the TreeInode object.
   */
  InodeMap* getInodeMap() const;

  /**
   * The ObjectStore is guaranteed to remain valid for at least the lifetime of
   * the TreeInode object.  (The ObjectStore is owned by the EdenMount.)
   */
  ObjectStore* getStore() const;

  void registerInodeLoadComplete(
      folly::Future<std::unique_ptr<InodeBase>>& future,
      PathComponentPiece name,
      InodeNumber number);
  void inodeLoadComplete(
      PathComponentPiece childName,
      std::unique_ptr<InodeBase> childInode);

  folly::Future<std::unique_ptr<InodeBase>> startLoadingInodeNoThrow(
      const DirEntry& entry,
      PathComponentPiece name,
      ObjectFetchContext& context) noexcept;

  folly::Future<std::unique_ptr<InodeBase>> startLoadingInode(
      const DirEntry& entry,
      PathComponentPiece name,
      ObjectFetchContext& context);

  /**
   * Materialize this directory in the overlay.
   *
   * This is required whenever we are about to make a structural change
   * in the tree; renames, creation, deletion.
   */
  void materialize(const RenameLock* renameLock = nullptr);

  FOLLY_NODISCARD ImmediateFuture<folly::Unit> doRename(
      TreeRenameLocks&& locks,
      PathComponentPiece srcName,
      PathMap<DirEntry>::iterator srcIter,
      TreeInodePtr destParent,
      PathComponentPiece destName,
      InvalidationRequired invalidate);

  Overlay* getOverlay() const;

  /**
   * Loads a tree from the overlay given an inode number.
   */
  DirContents loadOverlayDir(InodeNumber inodeNumber) const;

  /**
   * Saves the entries of this inode to the overlay.
   */
  void saveOverlayDir(const DirContents& contents) const;

  /**
   * Saves the entries for a specified inode number.
   */
  void saveOverlayDir(InodeNumber inodeNumber, const DirContents& contents)
      const;

  /**
   * Converts a Tree to a Dir and saves it to the Overlay under the given inode
   * number.
   */
  static DirContents
  saveDirFromTree(InodeNumber inodeNumber, const Tree* tree, EdenMount* mount);

  /** Translates a Tree object from our store into a Dir object
   * used to track the directory in the inode */
  static DirContents buildDirFromTree(
      const Tree* tree,
      Overlay* overlay,
      CaseSensitivity caseSensitive);

  void updateAtime();

  void prefetch(ObjectFetchContext& context);

  /**
   * Get a TreeInodePtr to ourself.
   *
   * This uses TreeInodePtr::newPtrFromExisting() internally.
   *
   * This should only be called in contexts where we know an external caller
   * already has an existing reference to us.  (Which is most places--a caller
   * has to have a reference to us in order to call any of our APIs.)
   */
  TreeInodePtr inodePtrFromThis() {
    return TreeInodePtr::newPtrFromExisting(this);
  }

  /**
   * Helper function to implement both fuseReaddir and nfsReaddir.
   *
   * Returns a boolean that indicates if readdir finished reading the entire
   * directory.
   */
  template <typename Fn>
  bool readdirImpl(off_t offset, ObjectFetchContext& context, Fn add);

  /**
   * createImpl() is a helper function for creating new children inodes.
   *
   * This is used by create(), symlink(), and mknod().
   */
  FileInodePtr createImpl(
      folly::Synchronized<TreeInodeState>::LockedPtr contentsLock,
      PathComponentPiece name,
      mode_t mode,
      folly::ByteRange fileContents,
      InvalidationRequired invalidate);

  /**
   * removeImpl() is the actual implementation used for unlink() and rmdir().
   *
   * The child inode in question must already be loaded.  removeImpl() will
   * confirm that this is still the correct inode for the given name, and
   * remove it if so.  If not it will attempt to load the child again, and will
   * retry the remove again (hence the attemptNum parameter).
   */
  template <typename InodePtrType>
  FOLLY_NODISCARD ImmediateFuture<folly::Unit> removeImpl(
      PathComponent name,
      InodePtr child,
      InvalidationRequired invalidate,
      unsigned int attemptNum,
      ObjectFetchContext& fetchContext);

  /**
   * tryRemoveChild() actually unlinks a child from our entry list.
   *
   * The caller must already be holding the mountpoint-wide RenameLock.
   *
   * This method also updates the overlay state if the child was removed
   * successfully.
   *
   * @param renameLock A reference to the rename lock (this parameter is
   *     required mostly to ensure that the caller is holding it).
   * @param name The entry name to remove.
   * @param child If this parameter is non-null, then only remove the entry if
   *     it refers to the specified inode.  If the entry does not refer to the
   *     inode in question, EBADF will be returned.
   * @param flushKernelCache This parameter indicates if we should tell the
   *     kernel to flush its cache for the removed entry.  This should always
   *     be set to true, unless tryRemoveChild() is being called from a FUSE
   *     unlink() or rmdir() call, in which case the kernel will update its
   *     cache automatically when the FUSE call returns.
   *
   * @return Returns an errno value on error, or 0 on success.  Notable errors
   * include:
   * - ENOENT: no entry exists the specified name
   * - EBADF: An entry exists with the specified name, but the InodeBase object
   *   for it is not loaded, or it does not refer to the same inode as the
   *   child parameter (if child was non-null).
   * - EISDIR: the entry with the specified name is a directory (only returned
   *   if InodePtrType is FileInodePtr).
   * - ENOTDIR: the entry with the specified name is not a directory (only
   *   returned if InodePtrType is TreeInodePtr).
   * - ENOTEMPTY: the directory being removed is not empty.
   *
   * Callers should assume that tryRemoveChild() may still throw an exception
   * on other unexpected error cases.
   */
  template <typename InodePtrType>
  FOLLY_NODISCARD int tryRemoveChild(
      const RenameLock& renameLock,
      PathComponentPiece name,
      InodePtrType child,
      InvalidationRequired invalidate);

  /**
   * checkPreRemove() is called by tryRemoveChild() for file or directory
   * specific checks before unlinking an entry.  Returns an errno value or 0.
   */
  FOLLY_NODISCARD static int checkPreRemove(const TreeInodePtr& child);
  FOLLY_NODISCARD static int checkPreRemove(const FileInodePtr& child);

  /**
   * Internal method intended for removeRecursively to use. This method does not
   * flush invalidation so the caller won't see the up-to-date content after
   * return. Call EdenMount::flushInvalidations to ensure any changes to the
   * inode will be visible after it returns.
   */
  ImmediateFuture<folly::Unit> removeRecursivelyNoFlushInvalidation(
      PathComponentPiece name,
      InvalidationRequired invalidate,
      ObjectFetchContext& context);

  /**
   * This helper function starts loading a currently unloaded child inode.
   * It must be held with the contents_ lock held.  (The Dir argument is only
   * required as a parameter to ensure that the caller is actually holding the
   * lock.)
   */
  folly::Future<InodePtr> loadChildLocked(
      DirContents& dir,
      PathComponentPiece name,
      DirEntry& entry,
      std::vector<IncompleteInodeLoad>& pendingLoads,
      ObjectFetchContext& fetchContext);

  /**
   * Load the .gitignore file for this directory, then call computeDiff() once
   * it is loaded.
   */
  FOLLY_NODISCARD folly::Future<folly::Unit> loadGitIgnoreThenDiff(
      InodePtr gitignoreInode,
      DiffContext* context,
      RelativePathPiece currentPath,
      std::shared_ptr<const Tree> tree,
      const GitIgnoreStack* parentIgnore,
      bool isIgnored);

  /**
   * The bulk of the actual implementation of diff()
   *
   * The main diff() function's GitIgnoreStack parameter contains the ignore
   * data for the ancestors of this directory.  diff() loads .gitignore data
   * for the current directory and then invokes computeDiff() to perform the
   * diff once all .gitignore data is loaded.
   */
  FOLLY_NODISCARD folly::Future<folly::Unit> computeDiff(
      folly::Synchronized<TreeInodeState>::LockedPtr contentsLock,
      DiffContext* context,
      RelativePathPiece currentPath,
      std::shared_ptr<const Tree> tree,
      std::unique_ptr<GitIgnoreStack> ignore,
      bool isIgnored);

  /**
   * Check to see if we can break out of a checkout() operation early.
   *
   * This should only be called for non-materialized TreeInodes that have a
   * source control hash.
   *
   * @param ctx The CheckoutContext
   * @param treeHash The source control hash for the TreeInode being updated.
   * @param fromTree The source control Tree that this checkout operation is
   *        moving away from.  This may be null if there was no source control
   *        state at this location previously.
   * @param toTree The destination source control Tree of the checkout.
   *        of the checkout).  This may be null if the destination state has no
   *        contents under this directory.
   */
  static bool canShortCircuitCheckout(
      CheckoutContext* ctx,
      const ObjectId& treeHash,
      const Tree* fromTree,
      const Tree* toTree);
  void computeCheckoutActions(
      CheckoutContext* ctx,
      const Tree* fromTree,
      const Tree* toTree,
      std::vector<std::unique_ptr<CheckoutAction>>& actions,
      std::vector<IncompleteInodeLoad>& pendingLoads,
      bool& wasDirectoryListModified);
  /**
   * Sets wasDirectoryListModified true if this checkout entry operation has
   * modified the directory contents, which implies the return value is nullptr.
   *
   * This function could return a std::variant of InvalidationRequired and
   * std::unique_ptr<CheckoutAction> instead of setting a boolean.
   */
  std::unique_ptr<CheckoutAction> processCheckoutEntry(
      CheckoutContext* ctx,
      TreeInodeState& contents,
      const TreeEntry* oldScmEntry,
      const TreeEntry* newScmEntry,
      std::vector<IncompleteInodeLoad>& pendingLoads,
      bool& wasDirectoryListModified);
  void saveOverlayPostCheckout(CheckoutContext* ctx, const Tree* tree);

  /**
   * Send a request to the kernel to invalidate its directory cache for this
   * inode.  This is required when the child entry list has changed.
   * invalidateChannelEntryCache(state, name) only works if the entry name is
   * known to the channel (FUSE, PrjFS), which is not true for new entries.
   *
   * A TreeInodeState is required as a way to ensure that contents_ lock is
   * being held to avoid races between invalidation during checkout and use
   * lookups.
   *
   * On NFS, we use the mode bits as part of invalidation. If this inode's
   * permission bits are updated, invalidateChannelDirCache must be called on
   * the parent inode afterwards.
   */
  FOLLY_NODISCARD ImmediateFuture<folly::Unit> invalidateChannelDirCache(
      TreeInodeState&);

  /**
   * Send a request to the kernel to invalidate its cache for the given child
   * entry name. On unices this corresponds to the dcache entry which caches
   * name lookups to child inodes. On Windows, this removes the on-disk
   * placeholder.
   *
   * This should be called when an entry is added, removed, or changed.
   * Invalidating upon removal is required because the kernel maintains a
   * negative cache on lookup failures on Unices.
   *
   * A TreeInodeState is required as a way to ensure that contents_ lock is
   * being held to avoid races between invalidation during checkout and use
   * lookups.
   */
  FOLLY_NODISCARD folly::Try<folly::Unit> invalidateChannelEntryCache(
      TreeInodeState&,
      PathComponentPiece name,
      std::optional<InodeNumber> ino);

  folly::Synchronized<TreeInodeState> contents_;

  /**
   * Only prefetch blob metadata on the first readdir() of a loaded inode.
   */
  std::atomic<bool> prefetched_{false};
};

/**
 * An internal function which computes the difference between a Dir and a tree
 * as a set of strings starting with + and - followed by the entry name.
 */
std::optional<std::vector<std::string>> findEntryDifferences(
    const DirContents& dir,
    const Tree& tree);

} // namespace eden
} // namespace facebook
