// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    anyhow::{anyhow, Context},
    fidl_fuchsia_hardware_power_statecontrol::AdminProxy as PowerStateControlProxy,
    fuchsia_syslog::fx_log_err,
    futures::{channel::mpsc, prelude::*},
};

/// External controller that determines when the update attempt should reboot into the new system.
pub struct RebootController(mpsc::Receiver<ControlRequest>);

enum ControlRequest {
    #[allow(dead_code)]
    Unblock,
}

impl RebootController {
    /// Wait for the external controller to signal it is time for the reboot.
    pub(super) async fn wait_to_reboot(&mut self) {
        match self.0.next().await {
            Some(ControlRequest::Unblock) => {}
            None => {
                // unexpected, but the only reasonable action is to still unblock the reboot.
            }
        }
    }
}

/// Reboots the system, logging errors instead of failing.
pub(super) async fn reboot(proxy: &PowerStateControlProxy) {
    if let Err(e) = async move {
        use fidl_fuchsia_hardware_power_statecontrol::RebootReason;
        proxy
            .reboot(RebootReason::SystemUpdate)
            .await
            .context("while performing reboot call")?
            .map_err(fuchsia_zircon::Status::from_raw)
            .context("reboot responded with")
    }
    .await
    {
        fx_log_err!("error initiating reboot: {:#}", anyhow!(e));
    }
}
