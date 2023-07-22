// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Defines the structures needed for saving/restoring faascale-mem devices.

use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::Duration;

use snapshot::Persist;
use timerfd::{SetTimeFlags, TimerState};
use utils::vm_memory::GuestMemoryMmap;
use versionize::{VersionMap, Versionize, VersionizeResult};
use versionize_derive::Versionize;

use super::*;
use crate::devices::virtio::faascale_mem::device::{FaascaleMemStats, ConfigSpace, FaascaleMem};
use crate::devices::virtio::persist::VirtioDeviceState;
use crate::devices::virtio::{DeviceState, TYPE_FAASCALE_MEM};

#[derive(Clone, Versionize)]
// NOTICE: Any changes to this structure require a snapshot version bump.
pub struct FaascaleMemConfigSpaceState {
    num_pages: u32,
    actual_pages: u32,
}

#[derive(Clone, Versionize)]
// NOTICE: Any changes to this structure require a snapshot version bump.
pub struct FaascaleMemStatsState {
    swap_in: Option<u64>,
    swap_out: Option<u64>,
    major_faults: Option<u64>,
    minor_faults: Option<u64>,
    free_memory: Option<u64>,
    total_memory: Option<u64>,
    available_memory: Option<u64>,
    disk_caches: Option<u64>,
    hugetlb_allocations: Option<u64>,
    hugetlb_failures: Option<u64>,
}

impl FaascaleMemStatsState {
    fn from_stats(stats: &FaascaleMemStats) -> Self {
        Self {
            swap_in: stats.swap_in,
            swap_out: stats.swap_out,
            major_faults: stats.major_faults,
            minor_faults: stats.minor_faults,
            free_memory: stats.free_memory,
            total_memory: stats.total_memory,
            available_memory: stats.available_memory,
            disk_caches: stats.disk_caches,
            hugetlb_allocations: stats.hugetlb_allocations,
            hugetlb_failures: stats.hugetlb_failures,
        }
    }

    fn create_stats(&self) -> FaascaleMemStats {
        FaascaleMemStats {
            swap_in: self.swap_in,
            swap_out: self.swap_out,
            major_faults: self.major_faults,
            minor_faults: self.minor_faults,
            free_memory: self.free_memory,
            total_memory: self.total_memory,
            available_memory: self.available_memory,
            disk_caches: self.disk_caches,
            hugetlb_allocations: self.hugetlb_allocations,
            hugetlb_failures: self.hugetlb_failures,
        }
    }
}

#[derive(Clone, Versionize)]
// NOTICE: Any changes to this structure require a snapshot version bump.
pub struct FaascaleMemState {
    stats_polling_interval_s: u16,
    stats_desc_index: Option<u16>,
    latest_stats: FaascaleMemStatsState,
    config_space: FaascaleMemConfigSpaceState,
    virtio_state: VirtioDeviceState,
}

pub struct FaascaleMemConstructorArgs {
    pub mem: GuestMemoryMmap,
}

impl Persist<'_> for FaascaleMem {
    type State = FaascaleMemState;
    type ConstructorArgs = FaascaleMemConstructorArgs;
    type Error = super::Error;

    fn save(&self) -> Self::State {
        FaascaleMemState {
            stats_polling_interval_s: self.stats_polling_interval_s,
            stats_desc_index: self.stats_desc_index,
            latest_stats: FaascaleMemStatsState::from_stats(&self.latest_stats),
            config_space: FaascaleMemConfigSpaceState {
                num_pages: self.config_space.num_pages,
                actual_pages: self.config_space.actual_pages,
            },
            virtio_state: VirtioDeviceState::from_device(self),
        }
    }

    fn restore(
        constructor_args: Self::ConstructorArgs,
        state: &Self::State,
    ) -> std::result::Result<Self, Self::Error> {
        // We can safely create the faascale-mem with arbitrary flags and
        // num_pages because we will overwrite them after.
        let mut faascale_mem = FaascaleMem::new(state.stats_polling_interval_s, true)?;

        let mut num_queues = NUM_QUEUES;
        // As per the virtio 1.1 specification, the statistics queue
        // should not exist if the statistics are not enabled.
        if state.stats_polling_interval_s == 0 {
            num_queues -= 1;
        }
        faascale_mem.queues = state
            .virtio_state
            .build_queues_checked(&constructor_args.mem, TYPE_FAASCALE_MEM, num_queues, QUEUE_SIZE)
            .map_err(|_| Self::Error::QueueRestoreError)?;
        faascale_mem.irq_trigger.irq_status =
            Arc::new(AtomicUsize::new(state.virtio_state.interrupt_status));
        faascale_mem.avail_features = state.virtio_state.avail_features;
        faascale_mem.acked_features = state.virtio_state.acked_features;
        faascale_mem.latest_stats = state.latest_stats.create_stats();
        faascale_mem.config_space = ConfigSpace {
            num_pages: state.config_space.num_pages,
            actual_pages: state.config_space.actual_pages,
        };

        if state.virtio_state.activated {
            faascale_mem.device_state = DeviceState::Activated(constructor_args.mem);

            if faascale_mem.stats_enabled() {
                // Restore the stats descriptor.
                faascale_mem.set_stats_desc_index(state.stats_desc_index);

                // Restart timer if needed.
                let timer_state = TimerState::Periodic {
                    current: Duration::from_secs(u64::from(state.stats_polling_interval_s)),
                    interval: Duration::from_secs(u64::from(state.stats_polling_interval_s)),
                };
                faascale_mem
                    .stats_timer
                    .set_state(timer_state, SetTimeFlags::Default);
            }
        }

        Ok(faascale_mem)
    }
}
