// Copyright 2020 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
mod aarch64;
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
mod x86_64;

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::convert::TryFrom;
use std::ops::{Deref, DerefMut};
use std::os::raw::{c_char, c_ulong};
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::Arc;

use libc::{open, O_CLOEXEC, O_RDWR};

use kvm_sys::*;
use sync::Mutex;
use sys_util::{
    errno_result, ioctl, ioctl_with_ref, ioctl_with_val, AsRawDescriptor, Error, FromRawDescriptor,
    GuestMemory, RawDescriptor, Result, SafeDescriptor,
};

use crate::{Hypervisor, HypervisorCap, MappedRegion, RunnableVcpu, Vcpu, VcpuExit, Vm};

// Wrapper around KVM_SET_USER_MEMORY_REGION ioctl, which creates, modifies, or deletes a mapping
// from guest physical to host user pages.
//
// Safe when the guest regions are guaranteed not to overlap.
unsafe fn set_user_memory_region(
    descriptor: &SafeDescriptor,
    slot: u32,
    read_only: bool,
    log_dirty_pages: bool,
    guest_addr: u64,
    memory_size: u64,
    userspace_addr: *mut u8,
) -> Result<()> {
    let mut flags = if read_only { KVM_MEM_READONLY } else { 0 };
    if log_dirty_pages {
        flags |= KVM_MEM_LOG_DIRTY_PAGES;
    }
    let region = kvm_userspace_memory_region {
        slot,
        flags,
        guest_phys_addr: guest_addr,
        memory_size,
        userspace_addr: userspace_addr as u64,
    };

    let ret = ioctl_with_ref(descriptor, KVM_SET_USER_MEMORY_REGION(), &region);
    if ret == 0 {
        Ok(())
    } else {
        errno_result()
    }
}

pub struct Kvm {
    kvm: SafeDescriptor,
}

type KvmCap = kvm::Cap;

impl Kvm {
    /// Opens `/dev/kvm/` and returns a Kvm object on success.
    pub fn new() -> Result<Kvm> {
        // Open calls are safe because we give a constant nul-terminated string and verify the
        // result.
        let ret = unsafe { open("/dev/kvm\0".as_ptr() as *const c_char, O_RDWR | O_CLOEXEC) };
        if ret < 0 {
            return errno_result();
        }
        // Safe because we verify that ret is valid and we own the fd.
        Ok(Kvm {
            kvm: unsafe { SafeDescriptor::from_raw_descriptor(ret) },
        })
    }
}

impl AsRawDescriptor for Kvm {
    fn as_raw_descriptor(&self) -> RawDescriptor {
        self.kvm.as_raw_descriptor()
    }
}

impl AsRawFd for Kvm {
    fn as_raw_fd(&self) -> RawFd {
        self.kvm.as_raw_descriptor()
    }
}

impl Hypervisor for Kvm {
    fn check_capability(&self, cap: &HypervisorCap) -> bool {
        if let Ok(kvm_cap) = KvmCap::try_from(cap) {
            // this ioctl is safe because we know this kvm descriptor is valid,
            // and we are copying over the kvm capability (u32) as a c_ulong value.
            unsafe { ioctl_with_val(self, KVM_CHECK_EXTENSION(), kvm_cap as c_ulong) == 1 }
        } else {
            // this capability cannot be converted on this platform, so return false
            false
        }
    }
}

// Used to invert the order when stored in a max-heap.
#[derive(Copy, Clone, Eq, PartialEq)]
struct MemSlot(u32);

impl Ord for MemSlot {
    fn cmp(&self, other: &MemSlot) -> Ordering {
        // Notice the order is inverted so the lowest magnitude slot has the highest priority in a
        // max-heap.
        other.0.cmp(&self.0)
    }
}

impl PartialOrd for MemSlot {
    fn partial_cmp(&self, other: &MemSlot) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A wrapper around creating and using a KVM VM.
pub struct KvmVm {
    vm: SafeDescriptor,
    guest_mem: GuestMemory,
    mem_regions: Arc<Mutex<HashMap<u32, Box<dyn MappedRegion>>>>,
    mem_slot_gaps: Arc<Mutex<BinaryHeap<MemSlot>>>,
}

impl KvmVm {
    /// Constructs a new `KvmVm` using the given `Kvm` instance.
    pub fn new(kvm: &Kvm, guest_mem: GuestMemory) -> Result<KvmVm> {
        // Safe because we know kvm is a real kvm fd as this module is the only one that can make
        // Kvm objects.
        let ret = unsafe { ioctl(kvm, KVM_CREATE_VM()) };
        if ret < 0 {
            return errno_result();
        }
        // Safe because we verify that ret is valid and we own the fd.
        let vm_descriptor = unsafe { SafeDescriptor::from_raw_descriptor(ret) };
        guest_mem.with_regions(|index, guest_addr, size, host_addr, _| {
            unsafe {
                // Safe because the guest regions are guaranteed not to overlap.
                set_user_memory_region(
                    &vm_descriptor,
                    index as u32,
                    false,
                    false,
                    guest_addr.offset() as u64,
                    size as u64,
                    host_addr as *mut u8,
                )
            }
        })?;
        // TODO(colindr/srichman): add default IRQ routes in IrqChip constructor or configure_vm
        Ok(KvmVm {
            vm: vm_descriptor,
            guest_mem,
            mem_regions: Arc::new(Mutex::new(HashMap::new())),
            mem_slot_gaps: Arc::new(Mutex::new(BinaryHeap::new())),
        })
    }

    fn create_kvm_vcpu(&self, _id: usize) -> Result<KvmVcpu> {
        Ok(KvmVcpu {})
    }
}

impl Vm for KvmVm {
    fn try_clone(&self) -> Result<Self> {
        Ok(KvmVm {
            vm: self.vm.try_clone()?,
            guest_mem: self.guest_mem.clone(),
            mem_regions: self.mem_regions.clone(),
            mem_slot_gaps: self.mem_slot_gaps.clone(),
        })
    }

    fn get_memory(&self) -> &GuestMemory {
        &self.guest_mem
    }
}

impl AsRawDescriptor for KvmVm {
    fn as_raw_descriptor(&self) -> RawDescriptor {
        self.vm.as_raw_descriptor()
    }
}

impl AsRawFd for KvmVm {
    fn as_raw_fd(&self) -> RawFd {
        self.vm.as_raw_descriptor()
    }
}

/// A wrapper around creating and using a KVM Vcpu.
pub struct KvmVcpu {}

impl Vcpu for KvmVcpu {
    type Runnable = RunnableKvmVcpu;

    fn to_runnable(self) -> Result<Self::Runnable> {
        Ok(RunnableKvmVcpu {
            vcpu: self,
            phantom: Default::default(),
        })
    }

    fn request_interrupt_window(&self) -> Result<()> {
        Ok(())
    }
}

/// A KvmVcpu that has a thread and can be run.
pub struct RunnableKvmVcpu {
    vcpu: KvmVcpu,

    // vcpus must stay on the same thread once they start.
    // Add the PhantomData pointer to ensure RunnableKvmVcpu is not `Send`.
    phantom: std::marker::PhantomData<*mut u8>,
}

impl RunnableVcpu for RunnableKvmVcpu {
    type Vcpu = KvmVcpu;

    fn run(&self) -> Result<VcpuExit> {
        Ok(VcpuExit::Unknown)
    }
}

impl Deref for RunnableKvmVcpu {
    type Target = <Self as RunnableVcpu>::Vcpu;

    fn deref(&self) -> &Self::Target {
        &self.vcpu
    }
}

impl DerefMut for RunnableKvmVcpu {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.vcpu
    }
}

impl<'a> TryFrom<&'a HypervisorCap> for KvmCap {
    type Error = Error;

    fn try_from(cap: &'a HypervisorCap) -> Result<KvmCap> {
        match cap {
            HypervisorCap::ArmPmuV3 => Ok(KvmCap::ArmPmuV3),
            HypervisorCap::ImmediateExit => Ok(KvmCap::ImmediateExit),
            HypervisorCap::S390UserSigp => Ok(KvmCap::S390UserSigp),
            HypervisorCap::TscDeadlineTimer => Ok(KvmCap::TscDeadlineTimer),
            HypervisorCap::UserMemory => Ok(KvmCap::UserMemory),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use sys_util::GuestAddress;

    #[test]
    fn new() {
        Kvm::new().unwrap();
    }

    #[test]
    fn check_capability() {
        let kvm = Kvm::new().unwrap();
        assert!(kvm.check_capability(&HypervisorCap::UserMemory));
        assert!(!kvm.check_capability(&HypervisorCap::S390UserSigp));
    }

    #[test]
    fn create_vm() {
        let kvm = Kvm::new().unwrap();
        let gm = GuestMemory::new(&[(GuestAddress(0), 0x1000)]).unwrap();
        KvmVm::new(&kvm, gm).unwrap();
    }

    #[test]
    fn clone_vm() {
        let kvm = Kvm::new().unwrap();
        let gm = GuestMemory::new(&[(GuestAddress(0), 0x1000)]).unwrap();
        let vm = KvmVm::new(&kvm, gm).unwrap();
        vm.try_clone().unwrap();
    }

    #[test]
    fn send_vm() {
        let kvm = Kvm::new().unwrap();
        let gm = GuestMemory::new(&[(GuestAddress(0), 0x1000)]).unwrap();
        let vm = KvmVm::new(&kvm, gm).unwrap();
        thread::spawn(move || {
            let _vm = vm;
        })
        .join()
        .unwrap();
    }

    #[test]
    fn get_memory() {
        let kvm = Kvm::new().unwrap();
        let gm = GuestMemory::new(&[(GuestAddress(0), 0x1000)]).unwrap();
        let vm = KvmVm::new(&kvm, gm).unwrap();
        let obj_addr = GuestAddress(0xf0);
        vm.get_memory().write_obj_at_addr(67u8, obj_addr).unwrap();
        let read_val: u8 = vm.get_memory().read_obj_from_addr(obj_addr).unwrap();
        assert_eq!(read_val, 67u8);
    }
}