// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::{
        builtin::capability::{BuiltinCapability, BuiltinCapabilityProvider},
        capability::*,
        model::{
            error::ModelError,
            hooks::{Event, EventPayload, Hook},
        },
    },
    anyhow::Error,
    async_trait::async_trait,
    cm_rust::CapabilityPath,
    fidl_fuchsia_kernel as fkernel,
    fuchsia_zircon::Resource,
    futures::prelude::*,
    lazy_static::lazy_static,
    std::{convert::TryInto, sync::Arc},
};

lazy_static! {
    static ref KERNEL_STATS_CAPABILITY_PATH: CapabilityPath =
        "/svc/fuchsia.kernel.Stats".try_into().unwrap();
}

/// An implementation of the `fuchsia.kernel.Stats` protocol.
pub struct KernelStats {
    resource: Resource,
}

impl KernelStats {
    /// `resource` must be the root resource.
    pub fn new(resource: Resource) -> Arc<Self> {
        Arc::new(Self { resource })
    }
}

#[async_trait]
impl BuiltinCapability for KernelStats {
    const NAME: &'static str = "KernelStats";
    type Marker = fkernel::StatsMarker;

    async fn serve(self: Arc<Self>, mut stream: fkernel::StatsRequestStream) -> Result<(), Error> {
        while let Some(stats_request) = stream.try_next().await? {
            match stats_request {
                fkernel::StatsRequest::GetMemoryStats { responder } => {
                    let mem_stats = &self.resource.mem_stats()?;
                    let stats = fkernel::MemoryStats {
                        total_bytes: Some(mem_stats.total_bytes),
                        free_bytes: Some(mem_stats.free_bytes),
                        wired_bytes: Some(mem_stats.wired_bytes),
                        total_heap_bytes: Some(mem_stats.total_heap_bytes),
                        free_heap_bytes: Some(mem_stats.free_heap_bytes),
                        vmo_bytes: Some(mem_stats.vmo_bytes),
                        mmu_overhead_bytes: Some(mem_stats.mmu_overhead_bytes),
                        ipc_bytes: Some(mem_stats.ipc_bytes),
                        other_bytes: Some(mem_stats.other_bytes),
                    };
                    responder.send(stats)?;
                }
                fkernel::StatsRequest::GetCpuStats { responder } => {
                    let cpu_stats = &self.resource.cpu_stats()?;
                    let mut per_cpu_stats: Vec<fkernel::PerCpuStats> =
                        Vec::with_capacity(cpu_stats.len());
                    for cpu_stat in cpu_stats.iter() {
                        per_cpu_stats.push(fkernel::PerCpuStats {
                            cpu_number: Some(cpu_stat.cpu_number),
                            flags: Some(cpu_stat.flags),
                            idle_time: Some(cpu_stat.idle_time),
                            reschedules: Some(cpu_stat.reschedules),
                            context_switches: Some(cpu_stat.context_switches),
                            irq_preempts: Some(cpu_stat.irq_preempts),
                            yields: Some(cpu_stat.yields),
                            ints: Some(cpu_stat.ints),
                            timer_ints: Some(cpu_stat.timer_ints),
                            timers: Some(cpu_stat.timers),
                            page_faults: Some(cpu_stat.page_faults),
                            exceptions: Some(cpu_stat.exceptions),
                            syscalls: Some(cpu_stat.syscalls),
                            reschedule_ipis: Some(cpu_stat.reschedule_ipis),
                            generic_ipis: Some(cpu_stat.generic_ipis),
                        });
                    }
                    let mut stats = fkernel::CpuStats {
                        actual_num_cpus: per_cpu_stats.len() as u64,
                        per_cpu_stats: Some(per_cpu_stats),
                    };
                    responder.send(&mut stats)?;
                }
            }
        }
        Ok(())
    }

    async fn on_framework_capability_routed<'a>(
        self: &'a Arc<Self>,
        capability: &'a InternalCapability,
        capability_provider: Option<Box<dyn CapabilityProvider>>,
    ) -> Result<Option<Box<dyn CapabilityProvider>>, ModelError> {
        match capability {
            InternalCapability::Protocol(capability_path)
                if *capability_path == *KERNEL_STATS_CAPABILITY_PATH =>
            {
                Ok(Some(Box::new(BuiltinCapabilityProvider::<KernelStats>::new(Arc::downgrade(
                    &self,
                )))))
            }
            _ => Ok(capability_provider),
        }
    }
}

#[async_trait]
impl Hook for KernelStats {
    async fn on(self: Arc<Self>, event: &Event) -> Result<(), ModelError> {
        if let Ok(EventPayload::CapabilityRouted {
            source: CapabilitySource::AboveRoot { capability },
            capability_provider,
        }) = &event.result
        {
            let mut provider = capability_provider.lock().await;
            *provider = self.on_framework_capability_routed(&capability, provider.take()).await?;
        };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*, fidl_fuchsia_boot as fboot, fuchsia_async as fasync,
        fuchsia_component::client::connect_to_service,
    };

    fn root_resource_available() -> bool {
        let bin = std::env::args().next();
        match bin.as_ref().map(String::as_ref) {
            Some("/pkg/test/component_manager_tests") => false,
            Some("/pkg/test/component_manager_boot_env_tests") => true,
            _ => panic!("Unexpected test binary name {:?}", bin),
        }
    }

    async fn get_root_resource() -> Result<Resource, Error> {
        let root_resource_provider = connect_to_service::<fboot::RootResourceMarker>()?;
        let root_resource_handle = root_resource_provider.get().await?;
        Ok(Resource::from(root_resource_handle))
    }

    async fn serve_kernel_stats() -> Result<fkernel::StatsProxy, Error> {
        let root_resource = get_root_resource().await?;

        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<fkernel::StatsMarker>()?;
        fasync::spawn_local(
            KernelStats::new(root_resource)
                .serve(stream)
                .unwrap_or_else(|e| panic!("Error while serving kernel stats: {}", e)),
        );
        Ok(proxy)
    }

    #[fasync::run_singlethreaded(test)]
    async fn get_mem_stats() -> Result<(), Error> {
        if !root_resource_available() {
            return Ok(());
        }

        let kernel_stats_provider = serve_kernel_stats().await?;
        let mem_stats = kernel_stats_provider.get_memory_stats().await?;

        assert!(mem_stats.total_bytes.unwrap() > 0);
        assert!(mem_stats.free_bytes.unwrap() > 0);
        assert!(mem_stats.wired_bytes.unwrap() > 0);
        assert!(mem_stats.total_heap_bytes.unwrap() > 0);
        assert!(mem_stats.free_heap_bytes.unwrap() > 0);
        assert!(mem_stats.vmo_bytes.unwrap() > 0);
        assert!(mem_stats.mmu_overhead_bytes.unwrap() > 0);
        assert!(mem_stats.ipc_bytes.unwrap() > 0);
        assert!(mem_stats.other_bytes.unwrap() > 0);

        Ok(())
    }

    #[fasync::run_singlethreaded(test)]
    async fn get_cpu_stats() -> Result<(), Error> {
        if !root_resource_available() {
            return Ok(());
        }

        let kernel_stats_provider = serve_kernel_stats().await?;
        let cpu_stats = kernel_stats_provider.get_cpu_stats().await?;
        let actual_num_cpus = cpu_stats.actual_num_cpus;
        assert!(actual_num_cpus > 0);
        let per_cpu_stats = cpu_stats.per_cpu_stats.unwrap();

        let mut idle_time_sum = 0;
        let mut reschedules_sum = 0;
        let mut context_switches_sum = 0;
        let mut irq_preempts_sum = 0;
        let mut yields_sum = 0;
        let mut ints_sum = 0;
        let mut timer_ints_sum = 0;
        let mut timers_sum = 0;
        let mut page_faults_sum = 0;
        let mut syscalls_sum = 0;
        let mut reschedule_ipis_sum = 0;

        for per_cpu_stat in per_cpu_stats.iter() {
            idle_time_sum += per_cpu_stat.idle_time.unwrap();
            reschedules_sum += per_cpu_stat.reschedules.unwrap();
            context_switches_sum += per_cpu_stat.context_switches.unwrap();
            irq_preempts_sum += per_cpu_stat.irq_preempts.unwrap();
            yields_sum += per_cpu_stat.yields.unwrap();
            ints_sum += per_cpu_stat.ints.unwrap();
            timer_ints_sum += per_cpu_stat.timer_ints.unwrap();
            timers_sum += per_cpu_stat.timers.unwrap();
            page_faults_sum += per_cpu_stat.page_faults.unwrap();
            syscalls_sum += per_cpu_stat.syscalls.unwrap();
            reschedule_ipis_sum += per_cpu_stat.reschedule_ipis.unwrap();
        }

        assert!(idle_time_sum > 0);
        assert!(reschedules_sum > 0);
        assert!(context_switches_sum > 0);
        assert!(irq_preempts_sum > 0);
        assert!(yields_sum > 0);
        assert!(ints_sum > 0);
        assert!(timer_ints_sum > 0);
        assert!(timers_sum > 0);
        assert!(page_faults_sum > 0);
        assert!(syscalls_sum > 0);
        assert!(reschedule_ipis_sum > 0);

        Ok(())
    }
}