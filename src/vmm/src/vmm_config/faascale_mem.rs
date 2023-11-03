// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

pub use crate::devices::virtio::faascale_mem::device::FaascaleMemStats;
pub use crate::devices::virtio::FAASCALE_MEM_DEV_ID;
use crate::devices::virtio::{FaascaleMem, FaascaleMemConfig};

type MutexFaascaleMem = Arc<Mutex<FaascaleMem>>;

/// Errors associated with the operations allowed on the faascale.
#[derive(Debug, derive_more::From)]
pub enum FaascaleMemConfigError {
    /// The user made a request on an inexistent faascale-mem device.
    DeviceNotFound,
    /// Device not activated yet.
    DeviceNotActive,
    /// The user tried to enable/disable the statistics after boot.
    InvalidStatsUpdate,
    /// Amount of pages requested is too large.
    TooManyPagesRequested,
    /// The user polled the statistics of a faascale-mem device that
    /// does not have the statistics enabled.
    StatsNotFound,
    /// Failed to create a faascale-mem device.
    CreateFailure(crate::devices::virtio::faascale_mem::Error),
    /// Failed to update the configuration of the ballon device.
    UpdateFailure(std::io::Error),
}

impl fmt::Display for FaascaleMemConfigError {
    fn fmt(&self, f: &mut fmt::Formatter) -> std::fmt::Result {
        use self::FaascaleMemConfigError::*;
        match self {
            DeviceNotFound => write!(f, "No faascale-mem device found."),
            DeviceNotActive => write!(
                f,
                "Device is inactive, check if faascale driver is enabled in guest kernel."
            ),
            InvalidStatsUpdate => write!(f, "Cannot enable/disable the statistics after boot."),
            TooManyPagesRequested => write!(f, "Amount of pages requested is too large."),
            StatsNotFound => write!(f, "Statistics for the faascale-mem device are not enabled"),
            CreateFailure(err) => write!(f, "Error creating the faascale-mem device: {:?}", err),
            UpdateFailure(err) => write!(
                f,
                "Error updating the faascale-mem device configuration: {:?}",
                err
            ),
        }
    }
}

type Result<T> = std::result::Result<T, FaascaleMemConfigError>;

/// This struct represents the strongly typed equivalent of the json body
/// from faascale-mem related requests.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FaascaleMemDeviceConfig {
    /// Interval in seconds between refreshing statistics.
    #[serde(default)]
    pub stats_polling_interval_s: u16,
    /// If need to pre alloc memory for faascale blocks
    #[serde(default)]
    pub pre_alloc_mem: bool,
    /// If need to pre handle tdp fault for faascale blocks
    #[serde(default)]
    pub pre_tdp_fault: bool,
}

impl From<FaascaleMemConfig> for FaascaleMemDeviceConfig {
    fn from(state: FaascaleMemConfig) -> Self {
        FaascaleMemDeviceConfig {
            stats_polling_interval_s: state.stats_polling_interval_s,
            pre_alloc_mem: state.pre_alloc_mem,
            pre_tdp_fault: state.pre_tdp_fault,
        }
    }
}


/// The data fed into a faascale-mem statistics interval update request.
/// Note that the state of the statistics cannot be changed from ON to OFF
/// or vice versa after boot, only the interval of polling can be changed
/// if the statistics were activated in the device configuration.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FaascaleMemUpdateStatsConfig {
    /// Interval in seconds between refreshing statistics.
    pub stats_polling_interval_s: u16,
}

/// A builder for `MutexFaascale` devices from 'FaascaleMemDeviceConfig'.
#[cfg_attr(not(test), derive(Default))]
pub struct FaascaleMemBuilder {
    inner: Option<MutexFaascaleMem>,
}

impl FaascaleMemBuilder {
    /// Creates an empty MutexFaascale Store.
    pub fn new() -> Self {
        Self { inner: None }
    }

    /// Inserts a MutexFaascale device in the store.
    /// If an entry already exists, it will overwrite it.
    pub fn set(&mut self, cfg: FaascaleMemDeviceConfig) -> Result<()> {
        self.inner = Some(Arc::new(Mutex::new(FaascaleMem::new(
            cfg.stats_polling_interval_s,
            // `restored` flag is false because this code path
            // is never called by snapshot restore functionality.
            false,
            cfg.pre_alloc_mem,
            cfg.pre_tdp_fault
        )?)));

        Ok(())
    }

    /// Inserts an existing faascale-mem device.
    pub fn set_device(&mut self, faascale_mem: MutexFaascaleMem) {
        self.inner = Some(faascale_mem);
    }

    /// Provides a reference to the MutexFaascale if present.
    pub fn get(&self) -> Option<&MutexFaascaleMem> {
        self.inner.as_ref()
    }

    /// Returns the same structure that was used to configure the device.
    pub fn get_config(&self) -> Result<FaascaleMemDeviceConfig> {
        self.get()
            .ok_or(FaascaleMemConfigError::DeviceNotFound)
            .map(|faascale_mutex| faascale_mutex.lock().expect("Poisoned lock").config())
            .map(FaascaleMemDeviceConfig::from)
    }
}
