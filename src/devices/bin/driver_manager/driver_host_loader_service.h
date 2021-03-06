// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BIN_DRIVER_MANAGER_DRIVER_HOST_LOADER_SERVICE_H_
#define SRC_DEVICES_BIN_DRIVER_MANAGER_DRIVER_HOST_LOADER_SERVICE_H_

#include <lib/fdio/namespace.h>
#include <lib/zx/channel.h>

#include <memory>

#include <fbl/unique_fd.h>
#include <loader-service/loader-service.h>

class SystemInstance;

// A loader service for driver_hosts that restricts access to dynamic libraries.
class DriverHostLoaderService {
 public:
  // Create a new loader service for driver_hosts. The |dispatcher| must have a
  // longer lifetime than |out|.
  static zx_status_t Create(async_dispatcher_t* dispatcher, SystemInstance* system_instance,
                            std::unique_ptr<DriverHostLoaderService>* out);
  ~DriverHostLoaderService();

  // Connect to the loader service.
  zx_status_t Connect(zx::channel* out);

  // Return the file descriptor for the root namespace of the loader service.
  const fbl::unique_fd& root() const { return root_; }

 private:
  fbl::unique_fd root_;
  loader_service_t* svc_ = nullptr;
};

#endif  // SRC_DEVICES_BIN_DRIVER_MANAGER_DRIVER_HOST_LOADER_SERVICE_H_
