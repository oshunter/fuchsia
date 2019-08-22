// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be found in the LICENSE file.

#include <fuchsia/feedback/cpp/fidl.h>
#include <lib/sys/cpp/service_directory.h>
#include <zircon/errors.h>

#include <memory>

#include "garnet/public/lib/fostr/fidl/fuchsia/feedback/formatting.h"
#include "src/developer/feedback/feedback_agent/constants.h"
#include "src/developer/feedback/testing/gmatchers.h"
#include "src/developer/feedback/utils/archive.h"
#include "third_party/googletest/googlemock/include/gmock/gmock.h"
#include "third_party/googletest/googletest/include/gtest/gtest.h"

namespace fuchsia {
namespace feedback {
namespace {

using ::feedback::MatchesKey;

// Smoke-tests the real environment service for the fuchsia.feedback.DataProvider FIDL interface,
// connecting through FIDL.
class FeedbackAgentIntegrationTest : public testing::Test {
 public:
  void SetUp() override { environment_services_ = ::sys::ServiceDirectory::CreateFromNamespace(); }

 protected:
  std::shared_ptr<::sys::ServiceDirectory> environment_services_;
};

TEST_F(FeedbackAgentIntegrationTest, ValidOverrideConfig_SmokeTest) {
  DataProviderSyncPtr data_provider;
  environment_services_->Connect(data_provider.NewRequest());

  DataProvider_GetData_Result out_result;
  ASSERT_EQ(data_provider->GetData(&out_result), ZX_OK);

  ASSERT_TRUE(out_result.is_response());

  // We cannot expect a particular value for each annotation or attachment because values might
  // depend on which device the test runs (e.g., board name) or what happened prior to running this
  // test (e.g., logs). But we should expect the keys to be present.
  //
  // We only expect the keys that are in configs/valid_override.json.
  ASSERT_TRUE(out_result.response().data.has_annotations());
  EXPECT_THAT(out_result.response().data.annotations(),
              testing::UnorderedElementsAreArray({
                  MatchesKey(kAnnotationBuildBoard),
                  MatchesKey(kAnnotationBuildLatestCommitDate),
                  MatchesKey(kAnnotationBuildProduct),
                  MatchesKey(kAnnotationBuildVersion),
              }));

  ASSERT_TRUE(out_result.response().data.has_attachments());
  EXPECT_THAT(out_result.response().data.attachments(), testing::UnorderedElementsAreArray({
                                                            MatchesKey(kAttachmentAnnotations),
                                                            MatchesKey(kAttachmentBuildSnapshot),
                                                        }));

  ASSERT_TRUE(out_result.response().data.has_attachment_bundle());
  const auto& attachment_bundle = out_result.response().data.attachment_bundle();
  EXPECT_STREQ(attachment_bundle.key.c_str(), kAttachmentBundle);
  std::vector<Attachment> unpacked_attachments;
  ASSERT_TRUE(::feedback::Unpack(attachment_bundle.value, &unpacked_attachments));
  EXPECT_THAT(unpacked_attachments, testing::UnorderedElementsAreArray({
                                        MatchesKey(kAttachmentAnnotations),
                                        MatchesKey(kAttachmentBuildSnapshot),
                                    }));
}

}  // namespace
}  // namespace feedback
}  // namespace fuchsia
