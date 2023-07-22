// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::io;

use utils::vm_memory::{GuestAddress, GuestMemory, GuestMemoryMmap, GuestMemoryRegion};

use super::{RemoveRegionError};

pub(crate) fn populate_range(
    guest_memory: &GuestMemoryMmap,
    range: (GuestAddress, u64),
    restored: bool,
) -> std::result::Result<(), RemoveRegionError> {
    let (guest_address, range_len) = range;

    if let Some(region) = guest_memory.find_region(guest_address) {
        if guest_address.0 + range_len > region.start_addr().0 + region.len() {
            return Err(RemoveRegionError::MalformedRange);
        }
        let phys_address = guest_memory
            .get_host_address(guest_address)
            .map_err(|_| RemoveRegionError::AddressTranslation)?;

        // Mmap a new anonymous region over the present one in order to create a hole.
        // This workaround is (only) needed after resuming from a snapshot because the guest memory
        // is mmaped from file as private and there is no `madvise` flag that works for this case.
        if restored {
            // SAFETY: The address and length are known to be valid.
            let ret = unsafe {
                libc::mmap(
                    phys_address.cast(),
                    range_len as usize,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_FIXED | libc::MAP_ANONYMOUS | libc::MAP_PRIVATE,
                    -1,
                    0,
                )
            };
            if ret == libc::MAP_FAILED {
                return Err(RemoveRegionError::MmapFail(io::Error::last_os_error()));
            }
        };

        unsafe {
            let range_len = range_len as usize;
            libc::memset(phys_address.cast(), 0 as libc::c_int, range_len);
            libc::memcpy(phys_address.cast(), "KINGDO".as_ptr() as *const libc::c_void, 6);
        };

        Ok(())
    } else {
        Err(RemoveRegionError::RegionNotFound)
    }
}

pub(crate) fn remove_range(
    guest_memory: &GuestMemoryMmap,
    range: (GuestAddress, u64),
    restored: bool,
) -> std::result::Result<(), RemoveRegionError> {
    let (guest_address, range_len) = range;

    if let Some(region) = guest_memory.find_region(guest_address) {
        if guest_address.0 + range_len > region.start_addr().0 + region.len() {
            return Err(RemoveRegionError::MalformedRange);
        }
        let phys_address = guest_memory
            .get_host_address(guest_address)
            .map_err(|_| RemoveRegionError::AddressTranslation)?;

        // Mmap a new anonymous region over the present one in order to create a hole.
        // This workaround is (only) needed after resuming from a snapshot because the guest memory
        // is mmaped from file as private and there is no `madvise` flag that works for this case.
        if restored {
            // SAFETY: The address and length are known to be valid.
            let ret = unsafe {
                libc::mmap(
                    phys_address.cast(),
                    range_len as usize,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_FIXED | libc::MAP_ANONYMOUS | libc::MAP_PRIVATE,
                    -1,
                    0,
                )
            };
            if ret == libc::MAP_FAILED {
                return Err(RemoveRegionError::MmapFail(io::Error::last_os_error()));
            }
        };

        // Madvise the region in order to mark it as not used.
        // SAFETY: The address and length are known to be valid.
        let ret = unsafe {
            let range_len = range_len as usize;
            libc::madvise(phys_address.cast(), range_len, libc::MADV_DONTNEED)
        };
        if ret < 0 {
            return Err(RemoveRegionError::MadviseFail(io::Error::last_os_error()));
        }

        Ok(())
    } else {
        Err(RemoveRegionError::RegionNotFound)
    }
}