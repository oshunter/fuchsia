// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "txn-group.h"

#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <zircon/assert.h>
#include <zircon/status.h>
#include <zircon/syscalls.h>

#include <ddk/debug.h>
#include <fbl/auto_lock.h>

TransactionGroup::TransactionGroup() : fifo_(ZX_HANDLE_INVALID), flags_(0), ctr_(0) {
  memset(&response_, 0, sizeof(response_));
}

TransactionGroup::~TransactionGroup() {}

void TransactionGroup::Initialize(zx_handle_t fifo, groupid_t group) {
  ZX_DEBUG_ASSERT(fifo_ == ZX_HANDLE_INVALID);
  fifo_ = fifo;
  response_.group = group;
}

zx_status_t TransactionGroup::Enqueue(bool do_respond, reqid_t reqid) {
  zx_status_t status = ZX_OK;
  fbl::AutoLock lock(&lock_);
  if (flags_ & kTxnFlagRespond) {
    // Shouldn't get more than one response for a txn.
    zxlogf(WARNING, "Attempted to request second responses for transaction group");
    response_.status = ZX_ERR_IO;
    status = ZX_ERR_IO;
  } else if (response_.status != ZX_OK) {
    // This operation already failed; don't bother processing it.
    zxlogf(WARNING, "Attempted to enqueue already failed transaction group: %s",
           zx_status_get_string(response_.status));
    status = ZX_ERR_IO;
  }
  ctr_++;
  if (do_respond) {
    response_.reqid = reqid;
    flags_ |= kTxnFlagRespond;
  }
  return status;
}

void TransactionGroup::CtrAdd(uint32_t n) {
  fbl::AutoLock lock(&lock_);
  ctr_ += n;
}

void TransactionGroup::Complete(zx_status_t status) {
  fbl::AutoLock lock(&lock_);
  if ((status != ZX_OK) && (response_.status == ZX_OK)) {
    zxlogf(WARNING, "Transaction completed with error status: %s", zx_status_get_string(status));
    response_.status = status;
  }

  response_.count++;
  ZX_DEBUG_ASSERT(ctr_ != 0);
  ZX_DEBUG_ASSERT(response_.count <= ctr_);

  if ((flags_ & kTxnFlagRespond) && (response_.count == ctr_)) {
    status = zx_fifo_write(fifo_, sizeof(response_), &response_, 1, nullptr);
    if (status != ZX_OK) {
      zxlogf(ERROR, "Block Server I/O error: Could not write response");
    }
    response_.count = 0;
    response_.status = ZX_OK;
    response_.reqid = 0;
    ctr_ = 0;
    flags_ &= ~kTxnFlagRespond;
  }
}
