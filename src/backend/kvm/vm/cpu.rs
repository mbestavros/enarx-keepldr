// SPDX-License-Identifier: Apache-2.0

use super::{KvmSegment, Vm};

use crate::backend::kvm::shim::{MemInfo, SYSCALL_TRIGGER_PORT};
use crate::backend::kvm::vm::image::x86::X86;
use crate::backend::kvm::vm::image::Arch;
use crate::backend::{Command, Thread};
use crate::sallyport::{Block, Reply};
use crate::syscall::{SYS_ENARX_BALLOON_MEMORY, SYS_ENARX_MEM_INFO};

use super::personality::Personality;

use anyhow::{anyhow, Result};
use kvm_ioctls::{VcpuExit, VcpuFd};
use primordial::Register;
use x86_64::registers::control::{Cr0Flags, Cr4Flags};
use x86_64::registers::model_specific::EferFlags;
use x86_64::PhysAddr;

use std::sync::{Arc, RwLock};

pub struct Cpu<A: Arch, P: Personality> {
    fd: VcpuFd,
    keep: Arc<RwLock<Vm<A, P>>>,
}

impl<P: Personality> Cpu<X86, P> {
    pub fn new(
        fd: VcpuFd,
        keep: Arc<RwLock<Vm<X86, P>>>,
        entry: PhysAddr,
        cr3: PhysAddr,
    ) -> Result<Self> {
        let mut cpu = Self { fd, keep };

        cpu.set_gen_regs(entry)?;
        cpu.set_special_regs(cr3)?;

        Ok(cpu)
    }

    fn set_gen_regs(&mut self, entry: PhysAddr) -> Result<()> {
        let mut regs = self.fd.get_regs()?;

        regs.rip = entry.as_u64();
        regs.rflags |= 0x2;

        self.fd.set_regs(&regs)?;
        Ok(())
    }

    fn set_special_regs(&mut self, cr3: PhysAddr) -> Result<()> {
        let mut sregs = self.fd.get_sregs()?;

        let cs = KvmSegment {
            base: 0,
            limit: 0xFFFFF,
            selector: 8,
            type_: 11,
            present: 1,
            dpl: 0,
            db: 0,
            s: 1,
            l: 1,
            g: 1,
            avl: 0,
            unusable: 0,
            padding: 0,
        };

        sregs.cs = cs;

        sregs.efer = (EferFlags::LONG_MODE_ENABLE | EferFlags::LONG_MODE_ACTIVE).bits();
        sregs.cr0 = (Cr0Flags::PROTECTED_MODE_ENABLE
            | Cr0Flags::NUMERIC_ERROR
            | Cr0Flags::PAGING
            | Cr0Flags::MONITOR_COPROCESSOR)
            .bits();
        sregs.cr3 = cr3.as_u64();
        sregs.cr4 = (Cr4Flags::PHYSICAL_ADDRESS_EXTENSION).bits();

        self.fd.set_sregs(&sregs)?;
        Ok(())
    }
}

impl<P: Personality> Thread for Cpu<X86, P> {
    fn enter(&mut self) -> Result<Command> {
        match self.fd.run()? {
            VcpuExit::IoOut(port, data) => match port {
                SYSCALL_TRIGGER_PORT => {
                    let mut keep = self.keep.write().unwrap();

                    debug_assert_eq!(data.len(), 2);
                    let block_nr = data[0] as usize + ((data[1] as usize) << 8);

                    let sallyport: &mut Block = unsafe {
                        std::slice::from_raw_parts_mut(
                            keep.syscall_blocks.start.as_mut_ptr(),
                            keep.syscall_blocks.count.get(),
                        )
                        .get_mut(block_nr)
                        .unwrap()
                    };

                    let syscall_nr: i64 = unsafe { sallyport.msg.req.num.into() };

                    match syscall_nr {
                        0..=512 => Ok(Command::SysCall(sallyport)),

                        SYS_ENARX_BALLOON_MEMORY => {
                            let pages = unsafe { sallyport.msg.req.arg[0].into() };

                            let result = keep.add_memory(pages).map(|addr| {
                                let ok_result: [Register<usize>; 2] = [addr.into(), 0.into()];
                                ok_result
                            })?;

                            sallyport.msg.rep = Reply::from(Ok(result));
                            Ok(Command::Continue)
                        }

                        SYS_ENARX_MEM_INFO => {
                            let mem_slots = keep.kvm.get_nr_memslots();
                            let virt_offset: i64 =
                                keep.regions.first().unwrap().as_virt().start.as_u64() as _;
                            let mem_info: MemInfo = MemInfo {
                                virt_offset,
                                mem_slots,
                            };

                            let c = sallyport.cursor();
                            c.write(&mem_info)
                                .map_err(|_| anyhow!("Failed to allocate MemInfo in Block"))?;

                            let ok_result: [Register<usize>; 2] = [0.into(), 0.into()];

                            sallyport.msg.rep = Reply::from(Ok(ok_result));

                            Ok(Command::Continue)
                        }

                        _ => unimplemented!(),
                    }
                }
                _ => Err(anyhow!("data from unexpected port: {}", port)),
            },
            exit_reason => {
                if cfg!(debug_assertions) {
                    Err(anyhow!("{:?} {:#x?}", exit_reason, self.fd.get_regs()))
                } else {
                    Err(anyhow!("{:?}", exit_reason))
                }
            }
        }
    }
}
