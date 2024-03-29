// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "factoryfs/fsck.h"

#include <block-client/cpp/fake-device.h>
#include <factoryfs/factoryfs.h>
#include <factoryfs/mkfs.h>
#include <zxtest/zxtest.h>

#include "utils.h"

using block_client::FakeBlockDevice;

namespace factoryfs {
namespace {
constexpr uint32_t kBlockSize = 512;
constexpr uint32_t kNumBlocks = 400 * kFactoryfsBlockSize / kBlockSize;

TEST(FsckTest, TestEmpty) {
  auto device = std::make_unique<FakeBlockDevice>(kNumBlocks, kBlockSize);
  ASSERT_TRUE(device);
  ASSERT_OK(FormatFilesystem(device.get()));

  MountOptions options;
  ASSERT_OK(Fsck(std::move(device), &options));
}

TEST(FsckTest, TestUnmountable) {
  auto device = std::make_unique<FakeBlockDevice>(kNumBlocks, kBlockSize);
  ASSERT_TRUE(device);

  MountOptions options;
  ASSERT_STATUS(Fsck(std::move(device), &options), ZX_ERR_IO_DATA_INTEGRITY);
}

TEST(FsckTest, TestCorrupted) {
  auto device = std::make_unique<FakeBlockDevice>(kNumBlocks, kBlockSize);
  ASSERT_TRUE(device);
  ASSERT_OK(FormatFilesystem(device.get()));

  Superblock info;
  DeviceBlockRead(device.get(), &info, sizeof(info), kSuperblockStart);
  info.magic = 0x0;
  DeviceBlockWrite(device.get(), &info, sizeof(info), kSuperblockStart);

  MountOptions options;
  ASSERT_STATUS(Fsck(std::move(device), &options), ZX_ERR_IO_DATA_INTEGRITY);
}

}  // namespace
}  // namespace factoryfs
