// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#pragma once

#include <cstdint>
#include <memory>
#include <vector>

#include "garnet/bin/media/media_service/media_service_impl.h"
#include "garnet/bin/media/util/fidl_publisher.h"
#include "garnet/bin/media/video/video_frame_source.h"
#include "lib/fidl/cpp/bindings/binding.h"
#include "lib/media/fidl/media_renderer.fidl.h"
#include "lib/media/fidl/video_renderer.fidl.h"
#include "lib/ui/scenic/client/host_image_cycler.h"
#include "lib/ui/view_framework/base_view.h"

namespace media {

// Fidl agent that renders video.
class VideoRendererImpl : public MediaServiceImpl::Product<VideoRenderer>,
                          public MediaRenderer,
                          public VideoRenderer {
 public:
  static std::shared_ptr<VideoRendererImpl> Create(
      f1dl::InterfaceRequest<VideoRenderer> video_renderer_request,
      f1dl::InterfaceRequest<MediaRenderer> media_renderer_request,
      MediaServiceImpl* owner);

  ~VideoRendererImpl() override;

 private:
  class View : public mozart::BaseView {
   public:
    View(mozart::ViewManagerPtr view_manager,
         f1dl::InterfaceRequest<mozart::ViewOwner> view_owner_request,
         std::shared_ptr<VideoFrameSource> video_frame_source);

    ~View() override;

   private:
    // |BaseView|:
    void OnSceneInvalidated(
        ui_mozart::PresentationInfoPtr presentation_info) override;

    std::shared_ptr<VideoFrameSource> video_frame_source_;
    TimelineFunction timeline_function_;

    scenic_lib::HostImageCycler image_cycler_;

    FXL_DISALLOW_COPY_AND_ASSIGN(View);
  };

  VideoRendererImpl(
      f1dl::InterfaceRequest<VideoRenderer> video_renderer_request,
      f1dl::InterfaceRequest<MediaRenderer> media_renderer_request,
      MediaServiceImpl* owner);

  // MediaRenderer implementation.
  void GetSupportedMediaTypes(
      const GetSupportedMediaTypesCallback& callback) override;

  void SetMediaType(MediaTypePtr media_type) override;

  void GetPacketConsumer(f1dl::InterfaceRequest<MediaPacketConsumer>
                             packet_consumer_request) override;

  void GetTimelineControlPoint(f1dl::InterfaceRequest<MediaTimelineControlPoint>
                                   control_point_request) override;

  // VideoRenderer implementation.
  void GetStatus(uint64_t version_last_seen,
                 const GetStatusCallback& callback) override;

  void CreateView(
      f1dl::InterfaceRequest<mozart::ViewOwner> view_owner_request) override;

  // Returns the media types supported by this video renderer.
  f1dl::Array<MediaTypeSetPtr> SupportedMediaTypes();

  f1dl::Binding<MediaRenderer> media_renderer_binding_;
  FidlPublisher<GetStatusCallback> status_publisher_;
  std::shared_ptr<VideoFrameSource> video_frame_source_;

  FXL_DISALLOW_COPY_AND_ASSIGN(VideoRendererImpl);
};

}  // namespace media
