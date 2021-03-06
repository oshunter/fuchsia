// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fcntl.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#include <string_view>

#include <fbl/unique_fd.h>

#include "src/storage/fs_test/fs_test_fixture.h"

namespace fs_test {
namespace {

using RwTest = FilesystemTest;

// Test that zero length read and write operations are valid.
TEST_P(RwTest, ZeroLengthOperations) {
  const std::string filename = GetPath("zero_length_ops");
  fbl::unique_fd fd(open(filename.c_str(), O_RDWR | O_CREAT, 0644));
  ASSERT_TRUE(fd);

  // Zero-length write.
  ASSERT_EQ(write(fd.get(), nullptr, 0), 0);
  ASSERT_EQ(pwrite(fd.get(), nullptr, 0, 0), 0);

  // Zero-length read.
  ASSERT_EQ(read(fd.get(), nullptr, 0), 0);
  ASSERT_EQ(pread(fd.get(), nullptr, 0, 0), 0);

  // Seek pointer unchanged.
  ASSERT_EQ(lseek(fd.get(), 0, SEEK_CUR), 0);

  ASSERT_EQ(close(fd.release()), 0);
  ASSERT_EQ(unlink(filename.c_str()), 0);
}

// Test that non-zero length read_at and write_at operations are valid.
TEST_P(RwTest, OffsetOperations) {
  srand(0xDEADBEEF);

  constexpr size_t kBufferSize = PAGE_SIZE;
  uint8_t expected[kBufferSize];
  for (size_t i = 0; i < std::size(expected); i++) {
    expected[i] = static_cast<uint8_t>(rand());
  }

  struct TestOption {
    size_t write_start;
    size_t read_start;
    size_t expected_read_length;
  };

  TestOption options[] = {
      {0, 0, kBufferSize},
      {0, 1, kBufferSize - 1},
      {1, 0, kBufferSize},
      {1, 1, kBufferSize},
  };

  for (const auto& opt : options) {
    const std::string filename = GetPath("offset_ops");
    fbl::unique_fd fd(open(filename.c_str(), O_RDWR | O_CREAT, 0644));
    ASSERT_TRUE(fd);

    uint8_t buf[kBufferSize];
    memset(buf, 0, sizeof(buf));

    // 1) Write "kBufferSize" bytes at opt.write_start
    ASSERT_EQ(pwrite(fd.get(), expected, sizeof(expected), opt.write_start),
              static_cast<ssize_t>(sizeof(expected)));

    // 2) Read "kBufferSize" bytes at opt.read_start;
    //    actually read opt.expected_read_length bytes.
    ASSERT_EQ(pread(fd.get(), buf, sizeof(expected), opt.read_start),
              static_cast<ssize_t>(opt.expected_read_length));

    // 3) Verify the contents of the read matched, the seek
    //    pointer is unchanged, and the file size is correct.
    if (opt.write_start <= opt.read_start) {
      size_t read_skip = opt.read_start - opt.write_start;
      ASSERT_EQ(memcmp(buf, expected + read_skip, opt.expected_read_length), 0);
    } else {
      size_t write_skip = opt.write_start - opt.read_start;
      uint8_t zeroes[write_skip];
      memset(zeroes, 0, sizeof(zeroes));
      ASSERT_EQ(memcmp(buf, zeroes, write_skip), 0);
    }
    ASSERT_EQ(lseek(fd.get(), 0, SEEK_CUR), 0);
    struct stat st;
    ASSERT_EQ(fstat(fd.get(), &st), 0);
    ASSERT_EQ(st.st_size, static_cast<ssize_t>(opt.write_start + sizeof(expected)));

    ASSERT_EQ(close(fd.release()), 0);
    ASSERT_EQ(unlink(filename.c_str()), 0);
  }
}

using RwSparseTest = FilesystemTest;

TEST_P(RwSparseTest, MaxFileSize) {
  constexpr std::string_view kTestData = "hello";
  off_t offset = fs().GetTraits().max_file_size - kTestData.size();
  const std::string foo = GetPath("foo");
  {
    fbl::unique_fd fd(open(foo.c_str(), O_RDWR | O_CREAT, 0644));
    ASSERT_TRUE(fd);
    ASSERT_EQ(pwrite(fd.get(), kTestData.data(), kTestData.size(), offset),
              static_cast<ssize_t>(kTestData.size()));
    ASSERT_EQ(fsync(fd.get()), 0);  // Deliberate sync so that close is likely to unload the vnode.
    ASSERT_EQ(close(fd.release()), 0);
  }
  {
    fbl::unique_fd fd(open(foo.c_str(), O_RDONLY));
    ASSERT_TRUE(fd);
    uint8_t buf[kTestData.size()];
    ASSERT_EQ(pread(fd.get(), buf, kTestData.size(), offset),
              static_cast<ssize_t>(kTestData.size()));
    ASSERT_EQ(memcmp(buf, kTestData.data(), kTestData.size()), 0);
  }
}

INSTANTIATE_TEST_SUITE_P(/*no prefix*/, RwTest, testing::ValuesIn(AllTestFilesystems()),
                         testing::PrintToStringParamName());

// These tests will only work on a file system that supports sparse files.
INSTANTIATE_TEST_SUITE_P(
    /*no prefix*/, RwSparseTest,
    testing::ValuesIn(MapAndFilterAllTestFilesystems(
        [](const TestFilesystemOptions& options) -> std::optional<TestFilesystemOptions> {
          if (options.filesystem->GetTraits().supports_sparse_files) {
            return options;
          } else {
            return std::nullopt;
          }
        })),
    testing::PrintToStringParamName());

}  // namespace
}  // namespace fs_test
