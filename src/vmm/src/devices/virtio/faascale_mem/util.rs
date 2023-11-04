// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::io;

use utils::vm_memory::{GuestAddress, GuestMemory, GuestMemoryMmap, GuestMemoryRegion};

use super::{RemoveRegionError};

use utils::{ioctl_iow_nr, ioctl_ioc_nr};
use crate::builder::get_global_vm_fd;

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct kvm_userspace_prealloc_memory_region {
    pub guest_phys_addr: u64,
    pub memory_size: u64,
}
ioctl_iow_nr!(KVM_PREALLOC_USER_MEMORY_REGION,
    kvm_bindings::KVMIO,
    0x49,
    kvm_userspace_prealloc_memory_region);

pub(crate) fn populate_range(
    guest_memory: &GuestMemoryMmap,
    range: (GuestAddress, u64),
    restored: bool,
    pre_mem_alloc:bool,
    pre_tdp_alloc:bool
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
            //#################  touch every page in the range #################
            if pre_mem_alloc{
                let start_time = std::time::Instant::now();
                let ret = libc::madvise(phys_address.cast(), range_len, libc::MADV_POPULATE_WRITE);
                if ret < 0 {
                    return Err(RemoveRegionError::MadviseFail(io::Error::last_os_error()));
                }
                log::info!("pre-mem-alloc at guest_phys_addr:{} with memory_size:{}, took {}ms", guest_address.0, range_len as u64, start_time.elapsed().as_millis());
            }

            // ################# for testing by guest-kernel
            libc::memcpy(phys_address.cast(), "KINGDO".as_ptr() as *const libc::c_void, 6);

            //################# pre handle tdp-pagefault for per faascale-block-page #################
            if pre_tdp_alloc{
                let start_time = std::time::Instant::now();
                // ioctl syscall is disabled while vcpu is running, we should disable the seccomp filter,
                // details can be found in  https://github.com/firecracker-microvm/firecracker/blob/main/docs/seccompiler.md
                libc::ioctl(get_global_vm_fd(), KVM_PREALLOC_USER_MEMORY_REGION() as libc::c_int,
                            &kvm_userspace_prealloc_memory_region {
                                guest_phys_addr: guest_address.0,
                                memory_size: range_len as u64,
                            },
                );
                log::info!("pre-tdp-fault use vmfd({}), at guest_phys_addr:{} with memory_size:{}, took {}ms",get_global_vm_fd(), guest_address.0, range_len as u64, start_time.elapsed().as_millis());
            }
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