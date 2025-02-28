/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include "eden/fs/utils/PathFuncs.h"

namespace folly {
template <typename T>
class Future;
}

namespace facebook::eden {

class ObjectId;
class Hash20;
class Tree;
class DiffContext;
class GitIgnoreStack;
class RootId;

/**
 * Compute the diff between two roots.
 *
 * The caller is responsible for ensuring that the DiffContext remains valid
 * until the returned Future completes.
 *
 * The differences will be recorded using the callback inside the passed
 * DiffContext.
 */
folly::Future<folly::Unit>
diffRoots(DiffContext* context, const RootId& root1, const RootId& root2);

/**
 * Compute the diff between a source control Tree and the current directory
 * state. This function is called with the hashes of a source control tree
 * entry and an unmaterialized inode entry.
 *
 * The path argument specifies the path to these trees, and will be prefixed
 * to all differences recorded in the results.
 *
 * The caller is responsible for ensuring that the context remains valid
 * until the returned Future completes.
 *
 * The differences will be recorded using the callback inside the passed
 * DiffContext.
 */
folly::Future<folly::Unit> diffTrees(
    DiffContext* context,
    RelativePathPiece currentPath,
    ObjectId scmHash,
    ObjectId wdHash,
    const GitIgnoreStack* parentIgnore,
    bool isIgnored);

/**
 * Process an added tree (present locally but not present in the source control
 * tree). This function is called with the hash of an unmaterialized inode
 * entry. This whole subtree is marked as added using the DiffContext.
 *
 * The path argument specifies the path to these trees, and will be prefixed
 * to all differences recorded in the results.
 *
 * The caller is responsible for ensuring that the context remains valid
 * until the returned Future completes.
 *
 * The differences will be recorded using the callback inside the passed
 * DiffContext.
 */
folly::Future<folly::Unit> diffAddedTree(
    DiffContext* context,
    RelativePathPiece currentPath,
    ObjectId wdHash,
    const GitIgnoreStack* ignore,
    bool isIgnored);

/**
 * Process a removed tree (not present locally but present in the source control
 * tree). This function is called with the hash of the source control tree
 * entry. This whole subtree is marked as removed using the DiffContext.
 *
 * The path argument specifies the path to these trees, and will be prefixed
 * to all differences recorded in the results.
 *
 * The caller is responsible for ensuring that the context remains valid
 * until the returned Future completes.
 *
 * The differences will be recorded using the callback inside the passed
 * DiffContext.
 */
folly::Future<folly::Unit> diffRemovedTree(
    DiffContext* context,
    RelativePathPiece currentPath,
    ObjectId scmHash);

} // namespace facebook::eden
