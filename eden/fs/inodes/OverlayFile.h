/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <folly/Expected.h>
#include <folly/File.h>
#include <folly/portability/SysUio.h>

namespace folly {
class File;
}

namespace facebook::eden {

class Overlay;

class OverlayFile {
 public:
  OverlayFile() = default;
  explicit OverlayFile(folly::File file, std::weak_ptr<Overlay> overlay);

  OverlayFile(OverlayFile&&) = default;
  OverlayFile& operator=(OverlayFile&&) = default;

  folly::Expected<struct stat, int> fstat() const;
  folly::Expected<ssize_t, int> preadNoInt(void* buf, size_t n, off_t offset)
      const;
  folly::Expected<off_t, int> lseek(off_t offset, int whence) const;
  folly::Expected<ssize_t, int>
  pwritev(const iovec* iov, int iovcnt, off_t offset) const;
  folly::Expected<int, int> ftruncate(off_t length) const;
  folly::Expected<int, int> fsync() const;
  folly::Expected<int, int> fallocate(off_t offset, off_t length) const;
  folly::Expected<int, int> fdatasync() const;
  folly::Expected<std::string, int> readFile() const;

 private:
  OverlayFile(const OverlayFile&) = delete;
  OverlayFile& operator=(const OverlayFile&) = delete;

  folly::File file_;
  std::weak_ptr<Overlay> overlay_;
};
} // namespace facebook::eden
