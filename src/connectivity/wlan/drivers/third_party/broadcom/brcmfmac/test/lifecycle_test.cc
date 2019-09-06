/*
 * Copyright (c) 2019 The Fuchsia Authors
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY
 * SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION
 * OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF OR IN
 * CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 */

#include <stdio.h>

#include <algorithm>

#include <ddk/device.h>
#include <gtest/gtest.h>

#include "src/connectivity/wlan/drivers/testing/lib/sim-device/device.h"
#include "src/connectivity/wlan/drivers/third_party/broadcom/brcmfmac/common.h"
#include "src/connectivity/wlan/drivers/third_party/broadcom/brcmfmac/core.h"
#include "src/connectivity/wlan/drivers/third_party/broadcom/brcmfmac/sim_device.h"

namespace wlan {
namespace brcmfmac {
namespace {

std::pair<zx::channel, zx::channel> make_channel() {
  zx::channel local;
  zx::channel remote;
  zx::channel::create(0, &local, &remote);
  return {std::move(local), std::move(remote)};
}

TEST(LifecycleTest, StartStop) {
  auto env = std::make_shared<simulation::Environment>();
  auto dev_mgr = std::make_shared<simulation::FakeDevMgr>();

  std::unique_ptr<SimDevice> device;
  zx_status_t status = SimDevice::Create(nullptr, dev_mgr, env, &device);
  ASSERT_EQ(status, ZX_OK);
}

TEST(LifecycleTest, StartWithSmeChannel) {
  auto env = std::make_shared<simulation::Environment>();
  auto dev_mgr = std::make_shared<simulation::FakeDevMgr>();

  // Create PHY.
  std::unique_ptr<SimDevice> device;
  zx_status_t status = SimDevice::Create(nullptr, dev_mgr, env, &device);
  ASSERT_EQ(status, ZX_OK);
  EXPECT_EQ(dev_mgr->wlan_sim_device_get_num_devices(), static_cast<size_t>(1));

  // Create iface.
  auto [local, _remote] = make_channel();
  wlanphy_impl_create_iface_req_t create_iface_req{.role = WLAN_INFO_MAC_ROLE_CLIENT,
                                                   .sme_channel = local.get()};
  uint16_t iface_id;
  status = device->WlanphyImplCreateIface(&create_iface_req, &iface_id);
  ASSERT_EQ(status, ZX_OK);
  EXPECT_EQ(dev_mgr->wlan_sim_device_get_num_devices(), static_cast<size_t>(2));

  // Simulate start call from Fuchsia's generic wlanif-impl driver.
  auto iface = dev_mgr->find_device_by_proto_id(ZX_PROTOCOL_WLANIF_IMPL);
  ASSERT_TRUE(iface.has_value());
  void* ctx = iface->dev_args.ctx;
  auto* iface_ops = static_cast<wlanif_impl_protocol_ops_t*>(iface->dev_args.proto_ops);
  zx_handle_t sme_channel = ZX_HANDLE_INVALID;
  wlanif_impl_ifc_t ifc_ops{};
  status = iface_ops->start(ctx, &ifc_ops, &sme_channel, nullptr);
  EXPECT_EQ(status, ZX_OK);
  EXPECT_EQ(sme_channel, local.get());

  // Verify calling start again will fail with proper error code.
  sme_channel = ZX_HANDLE_INVALID;
  status = iface_ops->start(ctx, &ifc_ops, &sme_channel, nullptr);
  EXPECT_EQ(status, ZX_ERR_ALREADY_BOUND);
  EXPECT_EQ(sme_channel, ZX_HANDLE_INVALID);
}

}  // namespace
}  // namespace brcmfmac
}  // namespace wlan
