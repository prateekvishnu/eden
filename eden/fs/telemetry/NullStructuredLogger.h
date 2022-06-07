/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include "eden/fs/telemetry/StructuredLogger.h"

namespace facebook::eden {

class NullStructuredLogger final : public StructuredLogger {
 public:
  NullStructuredLogger() : StructuredLogger{false, SessionInfo{}} {}

 private:
  void logDynamicEvent(DynamicEvent) override {}
};

} // namespace facebook::eden
