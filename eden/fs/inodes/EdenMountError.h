/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <stdexcept>

namespace facebook::eden {

class EdenMountError : public std::runtime_error {
 public:
  explicit EdenMountError(const std::string& what) : std::runtime_error{what} {}
  explicit EdenMountError(const char* what) : std::runtime_error{what} {}
};

} // namespace facebook::eden
