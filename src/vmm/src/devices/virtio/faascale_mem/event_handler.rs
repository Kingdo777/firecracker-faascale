// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::os::unix::io::AsRawFd;

use event_manager::{EventOps, Events, MutEventSubscriber};
use logger::{debug, error, warn};
use utils::epoll::EventSet;

use crate::devices::report_faascale_mem_event_fail;
use crate::devices::virtio::faascale_mem::device::FaascaleMem;
use crate::devices::virtio::{VirtioDevice, DEPOPULATE_INDEX, POPULATE_INDEX, FAASCALE_STATS_INDEX};

impl FaascaleMem {
    fn register_runtime_events(&self, ops: &mut EventOps) {
        if let Err(err) = ops.add(Events::new(&self.queue_evts[POPULATE_INDEX], EventSet::IN)) {
            error!("Failed to register populate queue event: {}", err);
        }
        if let Err(err) = ops.add(Events::new(&self.queue_evts[DEPOPULATE_INDEX], EventSet::IN)) {
            error!("Failed to register depopulate queue event: {}", err);
        }
        if self.stats_enabled() {
            if let Err(err) = ops.add(Events::new(&self.queue_evts[FAASCALE_STATS_INDEX], EventSet::IN)) {
                error!("Failed to register stats queue event: {}", err);
            }
            if let Err(err) = ops.add(Events::new(&self.stats_timer, EventSet::IN)) {
                error!("Failed to register stats timerfd event: {}", err);
            }
        }
    }

    fn register_activate_event(&self, ops: &mut EventOps) {
        if let Err(err) = ops.add(Events::new(&self.activate_evt, EventSet::IN)) {
            error!("Failed to register activate event: {}", err);
        }
    }

    fn process_activate_event(&self, ops: &mut EventOps) {
        debug!("faascale-mem: activate event");
        if let Err(err) = self.activate_evt.read() {
            error!("Failed to consume faascale-mem activate event: {:?}", err);
        }
        self.register_runtime_events(ops);
        if let Err(err) = ops.remove(Events::new(&self.activate_evt, EventSet::IN)) {
            error!("Failed to un-register activate event: {}", err);
        }
    }
}

impl MutEventSubscriber for FaascaleMem {
    fn process(&mut self, event: Events, ops: &mut EventOps) {
        let source = event.fd();
        let event_set = event.event_set();
        let supported_events = EventSet::IN;

        if !supported_events.contains(event_set) {
            warn!(
                "Received unknown event: {:?} from source: {:?}",
                event_set, source
            );
            return;
        }

        if self.is_activated() {
            let virtq_populate_ev_fd = self.queue_evts[POPULATE_INDEX].as_raw_fd();
            let virtq_depopulate_ev_fd = self.queue_evts[DEPOPULATE_INDEX].as_raw_fd();
            let virtq_stats_ev_fd = self.queue_evts[FAASCALE_STATS_INDEX].as_raw_fd();
            let stats_timer_fd = self.stats_timer.as_raw_fd();
            let activate_fd = self.activate_evt.as_raw_fd();

            // Looks better than C style if/else if/else.
            match source {
                _ if source == virtq_populate_ev_fd => self
                    .process_populate_queue_event()
                    .unwrap_or_else(report_faascale_mem_event_fail),
                _ if source == virtq_depopulate_ev_fd => self
                    .process_depopulate_queue_event()
                    .unwrap_or_else(report_faascale_mem_event_fail),
                _ if source == virtq_stats_ev_fd => self
                    .process_stats_queue_event()
                    .unwrap_or_else(report_faascale_mem_event_fail),
                _ if source == stats_timer_fd => self
                    .process_stats_timer_event()
                    .unwrap_or_else(report_faascale_mem_event_fail),
                _ if activate_fd == source => self.process_activate_event(ops),
                _ => {
                    warn!("FaascaleMem: Spurious event received: {:?}", source);
                }
            };
        } else {
            warn!(
                "FaascaleMem: The device is not yet activated. Spurious event received: {:?}",
                source
            );
        }
    }

    fn init(&mut self, ops: &mut EventOps) {
        // This function can be called during different points in the device lifetime:
        //  - shortly after device creation,
        //  - on device activation (is-activated already true at this point),
        //  - on device restore from snapshot.
        if self.is_activated() {
            self.register_runtime_events(ops);
        } else {
            self.register_activate_event(ops);
        }
    }
}