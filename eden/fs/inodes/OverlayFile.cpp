/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#ifndef _WIN32

#include "eden/fs/inodes/OverlayFile.h"

#include <folly/FileUtil.h>

#include "eden/fs/inodes/Overlay.h"

namespace facebook::eden {

OverlayFile::OverlayFile(folly::File file, std::weak_ptr<Overlay> overlay)
    : file_{std::move(file)}, overlay_{overlay} {}

folly::Expected<struct stat, int> OverlayFile::fstat() const {
  std::shared_ptr<Overlay> overlay = overlay_.lock();
  if (!overlay) {
    return folly::makeUnexpected(EIO);
  }
  IORequest req{overlay.get()};

  struct stat st {};
  if (::fstat(file_.fd(), &st)) {
    return folly::makeUnexpected(errno);
  }
  return st;
}

folly::Expected<ssize_t, int>
OverlayFile::preadNoInt(void* buf, size_t n, off_t offset) const {
  std::shared_ptr<Overlay> overlay = overlay_.lock();
  if (!overlay) {
    return folly::makeUnexpected(EIO);
  }
  IORequest req{overlay.get()};

  auto ret = folly::preadNoInt(file_.fd(), buf, n, offset);
  if (ret == -1) {
    return folly::makeUnexpected(errno);
  }
  return ret;
}

folly::Expected<off_t, int> OverlayFile::lseek(off_t offset, int whence) const {
  std::shared_ptr<Overlay> overlay = overlay_.lock();
  if (!overlay) {
    return folly::makeUnexpected(EIO);
  }
  IORequest req{overlay.get()};

  auto ret = ::lseek(file_.fd(), offset, whence);
  if (ret == -1) {
    return folly::makeUnexpected(errno);
  }
  return ret;
}

folly::Expected<ssize_t, int>
OverlayFile::pwritev(const iovec* iov, int iovcnt, off_t offset) const {
  std::shared_ptr<Overlay> overlay = overlay_.lock();
  if (!overlay) {
    return folly::makeUnexpected(EIO);
  }
  IORequest req{overlay.get()};

  auto ret = folly::pwritevNoInt(file_.fd(), iov, iovcnt, offset);
  if (ret == -1) {
    return folly::makeUnexpected(errno);
  }
  return ret;
}

folly::Expected<int, int> OverlayFile::ftruncate(off_t length) const {
  std::shared_ptr<Overlay> overlay = overlay_.lock();
  if (!overlay) {
    return folly::makeUnexpected(EIO);
  }
  IORequest req{overlay.get()};

  auto ret = ::ftruncate(file_.fd(), length);
  if (ret == -1) {
    return folly::makeUnexpected(errno);
  }
  return folly::makeExpected<int>(ret);
}

folly::Expected<int, int> OverlayFile::fsync() const {
  std::shared_ptr<Overlay> overlay = overlay_.lock();
  if (!overlay) {
    return folly::makeUnexpected(EIO);
  }
  IORequest req{overlay.get()};

  auto ret = ::fsync(file_.fd());
  if (ret == -1) {
    return folly::makeUnexpected(errno);
  }
  return folly::makeExpected<int>(ret);
}

folly::Expected<int, int> OverlayFile::fallocate(off_t offset, off_t length)
    const {
#ifdef __linux__
  std::shared_ptr<Overlay> overlay = overlay_.lock();
  if (!overlay) {
    return folly::makeUnexpected(EIO);
  }
  IORequest req{overlay.get()};

  // Don't use posix_fallocate, because glibc may try to emulate it with writes
  // to each chunk, and we definitely don't want that.
  auto ret = ::fallocate(file_.fd(), 0, offset, length);
  if (ret == -1) {
    return folly::makeUnexpected(errno);
  }
  return folly::makeExpected<int>(ret);
#else
  (void)offset;
  (void)length;
  return folly::makeUnexpected(ENOSYS);
#endif
}

folly::Expected<int, int> OverlayFile::fdatasync() const {
#ifndef __APPLE__
  std::shared_ptr<Overlay> overlay = overlay_.lock();
  if (!overlay) {
    return folly::makeUnexpected(EIO);
  }
  IORequest req{overlay.get()};

  auto ret = ::fdatasync(file_.fd());
  if (ret == -1) {
    return folly::makeUnexpected(errno);
  }
  return folly::makeExpected<int>(ret);
#else
  return fsync();
#endif
}

folly::Expected<std::string, int> OverlayFile::readFile() const {
  std::shared_ptr<Overlay> overlay = overlay_.lock();
  if (!overlay) {
    return folly::makeUnexpected(EIO);
  }
  IORequest req{overlay.get()};

  std::string out;
  if (!folly::readFile(file_.fd(), out)) {
    return folly::makeUnexpected(errno);
  }
  return folly::makeExpected<int>(std::move(out));
}

} // namespace facebook::eden

#endif
