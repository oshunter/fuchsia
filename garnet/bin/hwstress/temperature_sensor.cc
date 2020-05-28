// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "temperature_sensor.h"

#include <fuchsia/hardware/thermal/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/fdio/directory.h>
#include <lib/zx/status.h>
#include <stdio.h>
#include <unistd.h>
#include <zircon/assert.h>
#include <zircon/status.h>
#include <zircon/syscalls.h>

#include <string>
#include <string_view>
#include <utility>

#include <fbl/string.h>
#include <fbl/string_printf.h>
#include <fbl/unique_fd.h>

#include "status.h"
#include "util.h"

namespace hwstress {

namespace {

// Open the given path as a FIDL channel.
zx::status<zx::channel> OpenDeviceChannel(std::string_view path) {
  zx::channel client, server;
  zx_status_t status = zx::channel::create(0, &client, &server);
  if (status != ZX_OK) {
    return zx::error(status);
  }

  status = fdio_service_connect(std::string(path).c_str(), server.release());
  if (status != ZX_OK) {
    return zx::error(status);
  }

  return zx::success(std::move(client));
}

}  // namespace

class SystemTemperatureSensor : public TemperatureSensor {
 public:
  explicit SystemTemperatureSensor(fuchsia::hardware::thermal::DeviceSyncPtr channel)
      : channel_(std::move(channel)) {}

  // |TemperatureSensor| implementation.
  std::optional<double> ReadCelcius() override {
    float value;
    zx_status_t status2;
    zx_status_t status = channel_->GetTemperatureCelsius(&status2, &value);
    if (status != ZX_OK || status2 != ZX_OK) {
      return std::nullopt;
    }

    return value;
  }

 private:
  fuchsia::hardware::thermal::DeviceSyncPtr channel_;
};

std::unique_ptr<TemperatureSensor> CreateSystemTemperatureSensor(std::string_view device_path) {
  zx::status<zx::channel> channel = OpenDeviceChannel(device_path);
  if (channel.is_error()) {
    fprintf(stderr, "Could not open device: %s\n", channel.status_string());
    return nullptr;
  }
  return CreateSystemTemperatureSensor(std::move(channel.value()));
}

std::unique_ptr<TemperatureSensor> CreateSystemTemperatureSensor(zx::channel channel) {
  fuchsia::hardware::thermal::DeviceSyncPtr device{};
  device.Bind(std::move(channel));
  return std::make_unique<SystemTemperatureSensor>(std::move(device));
}

class NullTemperatureSensor : public TemperatureSensor {
  std::optional<double> ReadCelcius() override { return std::nullopt; }
};

std::unique_ptr<TemperatureSensor> CreateNullTemperatureSensor() {
  return std::make_unique<NullTemperatureSensor>();
}

TemperatureSensor* GetNullTemperatureSensor() {
  static NullTemperatureSensor sensor;
  return &sensor;
}

fbl::String TemperatureToString(std::optional<double> temperature) {
  return temperature.has_value() ? fbl::StringPrintf("%0.1f°C", temperature.value())
                                 : fbl::String("unknown");
}

}  // namespace hwstress