/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once
#include <folly/portability/SysTypes.h>
#ifndef _WIN32
#include <gtest/gtest_prod.h>
#endif

#include "eden/fs/utils/PathFuncs.h"

namespace facebook::eden {

/**
 * UserInfo contains information about the user running edenfs.
 *
 * This includes information such as the user ID, group ID, username, home
 * directory, etc.
 *
 * edenfs is intended to be invoked with root privileges, either using a setuid
 * binary or via sudo.  Once it starts it forks a small helper process that
 * retains root privileges, but the main process quickly drops privileges.
 *
 * getUserIdentity() determines the actual user privileges that edenfs
 * should use once it drops root privileges.
 */
class UserInfo {
 public:
  /**
   * Construct a UserInfo by looking up the user information for the currently
   * running program.
   */
  static UserInfo lookup();

  uid_t getUid() const {
    return uid_;
  }

  gid_t getGid() const {
    return gid_;
  }

  const std::string& getUsername() const {
    return username_;
  }

  const AbsolutePath& getHomeDirectory() const {
    return homeDirectory_;
  }

  /**
   * Update the home directory path.
   *
   * This is primarily intended to be used in unit tests.  In most other
   * situations we use the home directory detected initially by lookup().
   */
  void setHomeDirectory(AbsolutePathPiece path) {
    homeDirectory_ = path.copy();
  }

  /**
   * If the program is currently running with an effective user ID of root,
   * drop privileges to the information listed in this UserInfo object.
   *
   * If the program is not currently running with root privileges this function
   * will generally fail with a permissions exception (even if the current
   * privileges are already the same as those specified in the UserInfo
   * structure).
   */
  void dropPrivileges();

  // 65534 is commonly used for the "nobody" UID/GID.
  // This isn't universal, however, it still seems like a safer default
  // to use than root.
  static constexpr uid_t kDefaultNobodyUid = 65534;
  static constexpr gid_t kDefaultNobodyGid = 65534;

 private:
#ifndef _WIN32
  FRIEND_TEST(UserInfo, initFromSudo);
#endif

  UserInfo() {}

#ifndef _WIN32
  struct PasswdEntry;

  /**
   * Look up the passwd entry for the specified user ID.
   */
  static PasswdEntry getPasswdUid(uid_t uid);

  /**
   * Populate the UserInfo if getuid() returned a non-root UID.
   */
  void initFromNonRoot(uid_t uid);

  /**
   * Populate the UserInfo data from sudo information.
   *
   * Returns false if the SUDO_UID environment variable is not defined.
   * Throws an exception if SUDO_UID is defined but cannot be parsed or if
   * other necessary SUDO_* variables are missing.
   */
  bool initFromSudo();

  /**
   * Initialize the homeDirectory_.
   *
   * uid_ must already be set when initHomedir() is called.
   * The pwd argument points to a PasswdEntry if it has already been looked up,
   * or null if the PasswdEntry has not yet been looked up.
   */
  void initHomedir(PasswdEntry* pwd = nullptr);

  void restoreEnvironmentAfterSudo();
#endif // !_WIN32

  uid_t uid_{kDefaultNobodyUid};
  gid_t gid_{kDefaultNobodyGid};
  std::string username_;
  AbsolutePath homeDirectory_;
};

#ifndef _WIN32
/**
 * While EffectiveUserScope exists, the effective user ID and
 * effective group IDs are set to the invoking non-root user.  (But
 * the real user ID is temporarily set to root, even if run as a
 * setuid binary, so leaveEffectiveUserScope() can reset to the
 * original state.
 *
 * This is intended for use prior to calling
 * UserInfo::dropPrivileges().
 */
struct EffectiveUserScope {
 public:
  explicit EffectiveUserScope(const UserInfo& userInfo);
  ~EffectiveUserScope();

 private:
  explicit EffectiveUserScope() = delete;
  EffectiveUserScope(const EffectiveUserScope&) = delete;
  EffectiveUserScope(EffectiveUserScope&&) = delete;
  EffectiveUserScope& operator=(const EffectiveUserScope&) = delete;
  EffectiveUserScope& operator=(EffectiveUserScope&&) = delete;

  uid_t ruid_;
  uid_t euid_;
  gid_t rgid_;
  gid_t egid_;
};
#endif // !_WIN32
} // namespace facebook::eden
