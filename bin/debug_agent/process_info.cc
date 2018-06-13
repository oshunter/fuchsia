// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "garnet/bin/debug_agent/process_info.h"

#include <elf.h>
#include <lib/zx/thread.h>
#include <link.h>
#include <zircon/syscalls.h>
#include <zircon/syscalls/object.h>

#include "garnet/bin/debug_agent/object_util.h"
#include "lib/fxl/logging.h"

namespace debug_agent {

namespace {

constexpr size_t kMaxBuildIDSize = 64;

debug_ipc::ThreadRecord::State ThreadStateToEnum(uint32_t state) {
  struct Mapping {
    uint32_t int_state;
    debug_ipc::ThreadRecord::State enum_state;
  };
  static const Mapping mappings[] = {
      {ZX_THREAD_STATE_NEW, debug_ipc::ThreadRecord::State::kNew},
      {ZX_THREAD_STATE_RUNNING, debug_ipc::ThreadRecord::State::kRunning},
      {ZX_THREAD_STATE_SUSPENDED, debug_ipc::ThreadRecord::State::kSuspended},
      {ZX_THREAD_STATE_BLOCKED, debug_ipc::ThreadRecord::State::kBlocked},
      {ZX_THREAD_STATE_DYING, debug_ipc::ThreadRecord::State::kDying},
      {ZX_THREAD_STATE_DEAD, debug_ipc::ThreadRecord::State::kDead}};

// TODO(ZX-1843): This #ifdef is temporary to handle the transition.
// It can be deleted once the new version of zircon rolls out that has
// this macro.
#ifdef ZX_THREAD_STATE_BASIC
  state = ZX_THREAD_STATE_BASIC(state);
#endif

  for (const Mapping& mapping : mappings) {
    if (mapping.int_state == state)
      return mapping.enum_state;
  }
  FXL_NOTREACHED();
  return debug_ipc::ThreadRecord::State::kDead;
}

// Reads a null-terminated string from the given address of the given process.
zx_status_t ReadNullTerminatedString(const zx::process& process,
                                     zx_vaddr_t vaddr, std::string* dest) {
  // Max size of string we'll load as a sanity check.
  constexpr size_t kMaxString = 32768;

  dest->clear();

  constexpr size_t kBlockSize = 256;
  char block[kBlockSize];
  while (dest->size() < kMaxString) {
    size_t num_read = 0;
    zx_status_t status =
        process.read_memory(vaddr, block, kBlockSize, &num_read);
    if (status != ZX_OK)
      return status;

    for (size_t i = 0; i < num_read; i++) {
      if (block[i] == 0)
        return ZX_OK;
      dest->push_back(block[i]);
    }

    if (num_read < kBlockSize)
      return ZX_OK;  // Partial read: hit the mapped memory boundary.
    vaddr += kBlockSize;
  }
  return ZX_OK;
}

std::string GetBuildID(const zx::process& process, uint64_t base) {
  zx_vaddr_t vaddr = base;
  uint8_t tmp[4];

  size_t buf_size = kMaxBuildIDSize * 2 + 1;
  char buf[buf_size];

  size_t num_read = 0;
  zx_status_t status = process.read_memory(vaddr, tmp, 4, &num_read);
  if (status != ZX_OK)
    return std::string();
  if (memcmp(tmp, ELFMAG, SELFMAG))
    return std::string();

  Elf64_Off phoff;
  Elf64_Half num;
  status = process.read_memory(vaddr + offsetof(Elf64_Ehdr, e_phoff), &phoff,
                               sizeof(phoff), &num_read);
  if (status != ZX_OK)
    return std::string();
  status = process.read_memory(vaddr + offsetof(Elf64_Ehdr, e_phnum), &num,
                               sizeof(num), &num_read);
  if (status != ZX_OK)
    return std::string();

  for (Elf64_Half n = 0; n < num; n++) {
    zx_vaddr_t phaddr = vaddr + phoff + (n * sizeof(Elf64_Phdr));
    Elf64_Word type;
    status = process.read_memory(phaddr + offsetof(Elf64_Phdr, p_type), &type,
                                 sizeof(type), &num_read);
    if (status != ZX_OK)
      return std::string();
    if (type != PT_NOTE)
      continue;

    Elf64_Off off;
    Elf64_Xword size;
    status = process.read_memory(phaddr + offsetof(Elf64_Phdr, p_offset), &off,
                                 sizeof(off), &num_read);
    if (status != ZX_OK)
      return std::string();
    status = process.read_memory(phaddr + offsetof(Elf64_Phdr, p_filesz), &size,
                                 sizeof(size), &num_read);
    if (status != ZX_OK)
      return std::string();

    constexpr size_t kGnuSignatureSize = 4;
    const char kGnuSignature[kGnuSignatureSize] = "GNU";

    struct {
      Elf32_Nhdr hdr;
      char name[sizeof("GNU")];
    } hdr;

    while (size > sizeof(hdr)) {
      status = process.read_memory(vaddr + off, &hdr, sizeof(hdr), &num_read);
      if (status != ZX_OK)
        return std::string();
      size_t header_size = sizeof(Elf32_Nhdr) + ((hdr.hdr.n_namesz + 3) & -4);
      size_t payload_size = (hdr.hdr.n_descsz + 3) & -4;
      off += header_size;
      size -= header_size;
      zx_vaddr_t payload_vaddr = vaddr + off;
      off += payload_size;
      size -= payload_size;
      if (hdr.hdr.n_type != NT_GNU_BUILD_ID ||
          hdr.hdr.n_namesz != kGnuSignatureSize ||
          memcmp(hdr.name, kGnuSignature, kGnuSignatureSize) != 0) {
        continue;
      }
      if (hdr.hdr.n_descsz > kMaxBuildIDSize) {
        return std::string();  // Too large.
      } else {
        uint8_t buildid[kMaxBuildIDSize];
        status = process.read_memory(payload_vaddr, buildid, hdr.hdr.n_descsz,
                                     &num_read);
        if (status != ZX_OK)
          return std::string();
        for (uint32_t i = 0; i < hdr.hdr.n_descsz; ++i) {
          snprintf(&buf[i * 2], 3, "%02x", buildid[i]);
        }
      }
      return std::string(buf);
    }
  }

  return std::string();
}

}  // namespace

zx_status_t GetProcessInfo(zx_handle_t process, zx_info_process* info) {
  return zx_object_get_info(process, ZX_INFO_PROCESS, info,
                            sizeof(zx_info_process), nullptr, nullptr);
}

zx_status_t GetProcessThreads(zx_handle_t process,
                              std::vector<debug_ipc::ThreadRecord>* threads) {
  auto koids = GetChildKoids(process, ZX_INFO_PROCESS_THREADS);
  threads->resize(koids.size());
  for (size_t i = 0; i < koids.size(); i++) {
    (*threads)[i].koid = koids[i];

    zx_handle_t handle;
    if (zx_object_get_child(process, koids[i], ZX_RIGHT_SAME_RIGHTS, &handle) ==
        ZX_OK) {
      FillThreadRecord(zx::thread(handle), &(*threads)[i]);
    }
  }
  return ZX_OK;
}

void FillThreadRecord(const zx::thread& thread,
                      debug_ipc::ThreadRecord* record) {
  record->koid = KoidForObject(thread);
  record->name = NameForObject(thread);

  zx_info_thread info;
  if (thread.get_info(ZX_INFO_THREAD, &info, sizeof(info), nullptr, nullptr) ==
      ZX_OK) {
    record->state = ThreadStateToEnum(info.state);
  } else {
    FXL_NOTREACHED();
    record->state = debug_ipc::ThreadRecord::State::kDead;
  }
}

zx_status_t GetModulesForProcess(const zx::process& process,
                                 std::vector<debug_ipc::Module>* modules) {
  // The address of the link map in the process is stored in a property.
  uint64_t debug_addr = 0;
  zx_status_t status = process.get_property(ZX_PROP_PROCESS_DEBUG_ADDR,
                                            &debug_addr, sizeof(debug_addr));
  if (status != ZX_OK)
    return status;

  size_t num_read = 0;
  uint64_t lmap = 0;
  status = process.read_memory(debug_addr + offsetof(r_debug, r_map), &lmap,
                               sizeof(lmap), &num_read);
  if (status != ZX_OK)
    return status;

  // Walk the linked list.
  constexpr size_t kMaxObjects = 512;  // Sanity threshold.
  while (lmap != 0) {
    if (modules->size() >= kMaxObjects)
      return ZX_ERR_BAD_STATE;

    debug_ipc::Module module;
    if (process.read_memory(lmap + offsetof(link_map, l_addr), &module.base,
                            sizeof(module.base), &num_read) != ZX_OK)
      break;

    uint64_t next;
    if (process.read_memory(lmap + offsetof(link_map, l_next), &next,
                            sizeof(next), &num_read) != ZX_OK)
      break;

    uint64_t str_addr;
    if (process.read_memory(lmap + offsetof(link_map, l_name), &str_addr,
                            sizeof(str_addr), &num_read) != ZX_OK)
      break;

    if (ReadNullTerminatedString(process, str_addr, &module.name) != ZX_OK)
      break;

    module.build_id = GetBuildID(process, module.base);

    modules->push_back(std::move(module));
    lmap = next;
  }
  return ZX_OK;
}

}  // namespace debug_agent
