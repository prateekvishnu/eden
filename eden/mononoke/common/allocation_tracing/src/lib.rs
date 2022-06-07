/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

mod core;
mod futures_v1;

pub use crate::core::{trace_allocations, AllocationStats};
pub use crate::futures_v1::{
    AllocationTraced, AllocationTracingFutureExt, AllocationTracingStreamExt,
};
