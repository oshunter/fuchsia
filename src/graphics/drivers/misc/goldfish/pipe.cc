// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/graphics/drivers/misc/goldfish/pipe.h"

#include <fuchsia/hardware/goldfish/llcpp/fidl.h>
#include <lib/zx/bti.h>

#include <ddk/debug.h>
#include <ddk/trace/event.h>
#include <fbl/auto_lock.h>

namespace goldfish {
namespace {

static const char* kTag = "GoldfishPipe";

constexpr size_t DEFAULT_BUFFER_SIZE = 8192;

constexpr zx_signals_t SIGNALS = llcpp::fuchsia::hardware::goldfish::SIGNAL_READABLE |
                                 llcpp::fuchsia::hardware::goldfish::SIGNAL_WRITABLE;

}  // namespace

Pipe::Pipe(zx_device_t* parent, async_dispatcher_t* dispatcher, OnBindFn on_bind,
           OnCloseFn on_close)
    : on_bind_(std::move(on_bind)),
      on_close_(std::move(on_close)),
      dispatcher_(dispatcher),
      pipe_(parent) {}

Pipe::~Pipe() {
  fbl::AutoLock lock(&lock_);
  if (id_) {
    if (cmd_buffer_.is_valid()) {
      auto buffer = static_cast<pipe_cmd_buffer_t*>(cmd_buffer_.virt());
      buffer->id = id_;
      buffer->cmd = PIPE_CMD_CODE_CLOSE;
      buffer->status = PIPE_ERROR_INVAL;

      pipe_.Exec(id_);
      ZX_DEBUG_ASSERT(!buffer->status);
    }
    pipe_.Destroy(id_);
  }
}

void Pipe::Init() {
  fbl::AutoLock lock(&lock_);

  if (!pipe_.is_valid()) {
    FailAsync(ZX_ERR_BAD_STATE, "[%s] Pipe::Pipe() no pipe protocol", kTag);
    return;
  }

  zx_status_t status = pipe_.GetBti(&bti_);
  if (status != ZX_OK) {
    FailAsync(status, "[%s] Pipe::Pipe() GetBti failed", kTag);
    return;
  }

  status = SetBufferSizeLocked(DEFAULT_BUFFER_SIZE);
  if (status != ZX_OK) {
    FailAsync(status, "[%s] Pipe::Pipe() failed to set initial buffer size", kTag);
    return;
  }

  status = zx::event::create(0, &event_);
  if (status != ZX_OK) {
    FailAsync(status, "[%s] Pipe::Pipe() failed to create event", kTag);
    return;
  }
  status = event_.signal(0, SIGNALS);
  ZX_ASSERT(status == ZX_OK);

  zx::vmo vmo;
  goldfish_pipe_signal_value_t signal_cb = {Pipe::OnSignal, this};
  status = pipe_.Create(&signal_cb, &id_, &vmo);
  if (status != ZX_OK) {
    FailAsync(status, "[%s] Pipe::Pipe() failed to create pipe", kTag);
    return;
  }

  status = cmd_buffer_.InitVmo(bti_.get(), vmo.get(), 0, IO_BUFFER_RW);
  if (status != ZX_OK) {
    FailAsync(status, "[%s] Pipe::Pipe() io_buffer_init_vmo failed", kTag);
    return;
  }

  auto buffer = static_cast<pipe_cmd_buffer_t*>(cmd_buffer_.virt());
  buffer->id = id_;
  buffer->cmd = PIPE_CMD_CODE_OPEN;
  buffer->status = PIPE_ERROR_INVAL;

  pipe_.Open(id_);
  if (buffer->status) {
    FailAsync(ZX_ERR_INTERNAL, "[%s] Pipe::Pipe() failed to open pipe", kTag);
    cmd_buffer_.release();
  }
}

void Pipe::Bind(zx::channel server_request) {
  using PipeInterface = llcpp::fuchsia::hardware::goldfish::Pipe::Interface;
  auto on_unbound = [this](PipeInterface*, fidl::UnbindInfo info, zx::channel) {
    if (info.reason != fidl::UnbindInfo::kClose && info.reason != fidl::UnbindInfo::kUnbind) {
      zxlogf(ERROR, "[%s] Pipe error: %d\n", kTag, info.status);
    }
    if (on_close_) {
      on_close_(this);
    }
  };

  auto result =
      fidl::BindServer<PipeInterface>(dispatcher_, std::move(server_request),
                                      static_cast<PipeInterface*>(this), std::move(on_unbound));
  if (!result.is_ok()) {
    if (on_close_) {
      on_close_(this);
    }
  } else {
    binding_ref_ =
        std::make_unique<fidl::ServerBindingRef<llcpp::fuchsia::hardware::goldfish::Pipe>>(
            result.take_value());
  }
}

void Pipe::SetBufferSize(uint64_t size, SetBufferSizeCompleter::Sync completer) {
  TRACE_DURATION("gfx", "Pipe::SetBufferSize", "size", size);

  fbl::AutoLock lock(&lock_);

  zx_status_t status = SetBufferSizeLocked(size);
  if (status != ZX_OK) {
    zxlogf(ERROR, "[%s] Pipe::SetBufferSize() failed to create buffer: %lu", kTag, size);
    completer.Close(status);
  } else {
    completer.Reply(status);
  }
}

void Pipe::SetEvent(zx::event event, SetEventCompleter::Sync completer) {
  TRACE_DURATION("gfx", "Pipe::SetEvent");

  if (!event.is_valid()) {
    zxlogf(ERROR, "[%s] Pipe::SetEvent() invalid event", kTag);
    completer.Close(ZX_ERR_INVALID_ARGS);
    return;
  }

  fbl::AutoLock lock(&lock_);

  zx_handle_t observed = 0;
  zx_status_t status = event_.wait_one(SIGNALS, zx::time::infinite_past(), &observed);
  if (status != ZX_OK) {
    zxlogf(ERROR, "[%s] Pipe::SetEvent() failed to transfer observed signals: %d", kTag, status);
    completer.Close(status);
    return;
  }

  event_ = std::move(event);
  status = event_.signal(SIGNALS, observed & SIGNALS);
  ZX_ASSERT(status == ZX_OK);
  completer.Close(ZX_OK);
}

void Pipe::GetBuffer(GetBufferCompleter::Sync completer) {
  TRACE_DURATION("gfx", "Pipe::GetBuffer");

  fbl::AutoLock lock(&lock_);

  zx::vmo vmo;
  zx_status_t status = buffer_.vmo.duplicate(ZX_RIGHT_SAME_RIGHTS, &vmo);
  if (status != ZX_OK) {
    zxlogf(ERROR, "[%s] Pipe::GetBuffer() zx_vmo_duplicate failed: %d", kTag, status);
    completer.Close(status);
  } else {
    completer.Reply(ZX_OK, std::move(vmo));
  }
}

void Pipe::Read(uint64_t count, uint64_t offset, ReadCompleter::Sync completer) {
  TRACE_DURATION("gfx", "Pipe::Read", "count", count);

  fbl::AutoLock lock(&lock_);

  if ((offset + count) > buffer_.size || (offset + count) < offset) {
    completer.Close(ZX_ERR_INVALID_ARGS);
    return;
  }

  size_t actual;
  zx_status_t status = TransferLocked(PIPE_CMD_CODE_READ, PIPE_CMD_CODE_WAKE_ON_READ,
                                      llcpp::fuchsia::hardware::goldfish::SIGNAL_READABLE,
                                      buffer_.phys + offset, count, 0, 0, &actual);
  completer.Reply(status, actual);
}

void Pipe::Write(uint64_t count, uint64_t offset, WriteCompleter::Sync completer) {
  TRACE_DURATION("gfx", "Pipe::Write", "count", count);

  fbl::AutoLock lock(&lock_);

  if ((offset + count) > buffer_.size || (offset + count) < offset) {
    completer.Close(ZX_ERR_INVALID_ARGS);
    return;
  }

  size_t actual;
  zx_status_t status = TransferLocked(PIPE_CMD_CODE_WRITE, PIPE_CMD_CODE_WAKE_ON_WRITE,
                                      llcpp::fuchsia::hardware::goldfish::SIGNAL_WRITABLE,
                                      buffer_.phys + offset, count, 0, 0, &actual);
  completer.Reply(status, actual);
}

void Pipe::DoCall(uint64_t count, uint64_t offset, uint64_t read_count, uint64_t read_offset,
                  DoCallCompleter::Sync completer) {
  TRACE_DURATION("gfx", "Pipe::Call", "count", count, "read_count", read_count);

  fbl::AutoLock lock(&lock_);

  if ((offset + count) > buffer_.size || (offset + count) < offset) {
    completer.Close(ZX_ERR_INVALID_ARGS);
    return;
  }
  if ((read_offset + read_count) > buffer_.size || (read_offset + read_count) < read_offset) {
    completer.Close(ZX_ERR_INVALID_ARGS);
    return;
  }

  size_t remaining = count;
  size_t remaining_read = read_count;

  int32_t cmd = PIPE_CMD_CODE_WRITE;
  zx_paddr_t read_paddr = 0;
  if (read_count) {
    cmd = PIPE_CMD_CODE_CALL;
    read_paddr = buffer_.phys + read_offset;
  }

  // Blocking write. This should always make progress or fail.
  while (remaining) {
    size_t actual;
    zx_status_t status = TransferLocked(
        cmd, PIPE_CMD_CODE_WAKE_ON_WRITE, llcpp::fuchsia::hardware::goldfish::SIGNAL_WRITABLE,
        buffer_.phys + offset, remaining, read_paddr, read_count, &actual);
    if (status == ZX_OK) {
      // Calculate bytes written and bytes read. Adjust counts and offsets accordingly.
      size_t actual_write = std::min(actual, remaining);
      size_t actual_read = actual - actual_write;
      remaining -= actual_write;
      offset += actual_write;
      remaining_read -= actual_read;
      read_offset += actual_read;
      continue;
    }
    if (status != ZX_ERR_SHOULD_WAIT) {
      completer.Reply(status, 0);
      return;
    }
    signal_cvar_.Wait(&lock_);
  }

  // Non-blocking read if no data has been read yet.
  zx_status_t status = ZX_OK;
  if (read_count && remaining_read == read_count) {
    size_t actual = 0;
    status = TransferLocked(PIPE_CMD_CODE_READ, PIPE_CMD_CODE_WAKE_ON_READ,
                            llcpp::fuchsia::hardware::goldfish::SIGNAL_READABLE,
                            buffer_.phys + read_offset, remaining_read, 0, 0, &actual);
    if (status == ZX_OK) {
      remaining_read -= actual;
    }
  }
  size_t actual_read = read_count - remaining_read;
  completer.Reply(status, actual_read);
}

// This function can be trusted to complete fairly quickly. It will cause a
// VM exit but that should never block for a significant amount of time.
zx_status_t Pipe::TransferLocked(int32_t cmd, int32_t wake_cmd, zx_signals_t state_clr,
                                 zx_paddr_t paddr, size_t count, zx_paddr_t read_paddr,
                                 size_t read_count, size_t* actual) {
  TRACE_DURATION("gfx", "Pipe::Transfer", "count", count, "read_count", read_count);

  auto buffer = static_cast<pipe_cmd_buffer_t*>(cmd_buffer_.virt());
  buffer->id = id_;
  buffer->cmd = cmd;
  buffer->status = PIPE_ERROR_INVAL;
  buffer->rw_params.ptrs[0] = paddr;
  buffer->rw_params.sizes[0] = static_cast<uint32_t>(count);
  buffer->rw_params.ptrs[1] = read_paddr;
  buffer->rw_params.sizes[1] = static_cast<uint32_t>(read_count);
  buffer->rw_params.buffers_count = read_paddr ? 2 : 1;
  buffer->rw_params.consumed_size = 0;
  buffer->rw_params.read_index = 1;  // Read buffer is always second.
  pipe_.Exec(id_);

  // Positive consumed size always indicate a successful transfer.
  if (buffer->rw_params.consumed_size) {
    *actual = buffer->rw_params.consumed_size;
    return ZX_OK;
  }

  *actual = 0;
  // Early out if error is not because of back-pressure.
  if (buffer->status != PIPE_ERROR_AGAIN) {
    zxlogf(ERROR, "[%s] Pipe::Transfer() transfer failed: %d", kTag, buffer->status);
    return ZX_ERR_INTERNAL;
  }

  // PIPE_ERROR_AGAIN means that we need to wait until pipe is
  // readable/writable before we can perform another transfer command.
  // Clear event_ READABLE/WRITABLE bits and request an interrupt that
  // will indicate that the pipe is again readable/writable.
  event_.signal(state_clr, 0);

  buffer->id = id_;
  buffer->cmd = wake_cmd;
  buffer->status = PIPE_ERROR_INVAL;
  pipe_.Exec(id_);
  if (buffer->status) {
    zxlogf(ERROR, "[%s] Pipe::Transfer() failed to request interrupt: %d", kTag, buffer->status);
    return ZX_ERR_INTERNAL;
  }

  return ZX_ERR_SHOULD_WAIT;
}

zx_status_t Pipe::SetBufferSizeLocked(size_t size) {
  zx::vmo vmo;
  zx_status_t status = zx::vmo::create_contiguous(bti_, size, 0, &vmo);
  if (status != ZX_OK) {
    zxlogf(ERROR, "[%s] Pipe::CreateBuffer() zx_vmo_create_contiguous failed %d size: %zu", kTag,
           status, size);
    return status;
  }

  zx_paddr_t phys;
  zx::pmt pmt;
  // We leave pinned continuously, since buffer is expected to be used frequently.
  status = bti_.pin(ZX_BTI_PERM_READ | ZX_BTI_CONTIGUOUS, vmo, 0, size, &phys, 1, &pmt);
  if (status != ZX_OK) {
    zxlogf(ERROR, "[%s] Pipe::CreateBuffer() zx_bti_pin failed %d size: %zu", kTag, status, size);
    return status;
  }

  buffer_ = Buffer{.vmo = std::move(vmo), .pmt = std::move(pmt), .size = size, .phys = phys};
  return ZX_OK;
}

void Pipe::FailAsync(zx_status_t epitaph, const char* format, ...) {
  if (binding_ref_) {
    binding_ref_->Close(epitaph);
  }

  va_list args;
  va_start(args, format);
  zxlogvf(ERROR, format, args);
  va_end(args);
}

// static
void Pipe::OnSignal(void* ctx, int32_t flags) {
  TRACE_DURATION("gfx", "Pipe::OnSignal", "flags", flags);

  zx_signals_t state_set = 0;
  if (flags & PIPE_WAKE_FLAG_CLOSED) {
    state_set |= llcpp::fuchsia::hardware::goldfish::SIGNAL_HANGUP;
  }
  if (flags & PIPE_WAKE_FLAG_READ) {
    state_set |= llcpp::fuchsia::hardware::goldfish::SIGNAL_READABLE;
  }
  if (flags & PIPE_WAKE_FLAG_WRITE) {
    state_set |= llcpp::fuchsia::hardware::goldfish::SIGNAL_WRITABLE;
  }

  auto pipe = static_cast<Pipe*>(ctx);

  fbl::AutoLock lock(&pipe->lock_);
  // The event_ signal is for client code, while the signal_cvar_ is for this class.
  pipe->event_.signal(0, state_set);
  pipe->signal_cvar_.Signal();
}

Pipe::Buffer& Pipe::Buffer::operator=(Pipe::Buffer&& other) noexcept {
  if (pmt.is_valid()) {
    pmt.unpin();
  }
  vmo = std::move(other.vmo);
  pmt = std::move(other.pmt);
  phys = other.phys;
  size = other.size;
  return *this;
}

Pipe::Buffer::~Buffer() {
  if (pmt.is_valid()) {
    pmt.unpin();
  }
}

}  // namespace goldfish
