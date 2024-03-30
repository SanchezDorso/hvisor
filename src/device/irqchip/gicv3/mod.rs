// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Copyright (c) 2020-2022 Andre Richter <andre.o.richter@gmail.com>

//! GICv2 Driver - ARM Generic Interrupt Controller v2.
//!
//! The following is a collection of excerpts with useful information from
//!   - `Programmer's Guide for ARMv8-A`
//!   - `ARM Generic Interrupt Controller Architecture Specification`
//!
//! # Programmer's Guide - 10.6.1 Configuration
//!
//! The GIC is accessed as a memory-mapped peripheral.
//!
//! All cores can access the common Distributor, but the CPU interface is banked, that is, each core
//! uses the same address to access its own private CPU interface.
//!
//! It is not possible for a core to access the CPU interface of another core.
//!
//! # Architecture Specification - 10.6.2 Initialization
//!
//! Both the Distributor and the CPU interfaces are disabled at reset. The GIC must be initialized
//! after reset before it can deliver interrupts to the core.
//!
//! In the Distributor, software must configure the priority, target, security and enable individual
//! interrupts. The Distributor must subsequently be enabled through its control register
//! (GICD_CTLR). For each CPU interface, software must program the priority mask and preemption
//! settings.
//!
//! Each CPU interface block itself must be enabled through its control register (GICD_CTLR). This
//! prepares the GIC to deliver interrupts to the core.
//!
//! Before interrupts are expected in the core, software prepares the core to take interrupts by
//! setting a valid interrupt vector in the vector table, and clearing interrupt mask bits in
//! PSTATE, and setting the routing controls.
//!
//! The entire interrupt mechanism in the system can be disabled by disabling the Distributor.
//! Interrupt delivery to an individual core can be disabled by disabling its CPU interface.
//! Individual interrupts can also be disabled (or enabled) in the distributor.
//!
//! For an interrupt to reach the core, the individual interrupt, Distributor and CPU interface must
//! all be enabled. The interrupt also needs to be of sufficient priority, that is, higher than the
//! core's priority mask.
//!
//! # Architecture Specification - 1.4.2 Interrupt types
//!
//! - Peripheral interrupt
//!     - Private Peripheral Interrupt (PPI)
//!         - This is a peripheral interrupt that is specific to a single processor.
//!     - Shared Peripheral Interrupt (SPI)
//!         - This is a peripheral interrupt that the Distributor can route to any of a specified
//!           combination of processors.
//!
//! - Software-generated interrupt (SGI)
//!     - This is an interrupt generated by software writing to a GICD_SGIR register in the GIC. The
//!       system uses SGIs for interprocessor communication.
//!     - An SGI has edge-triggered properties. The software triggering of the interrupt is
//!       equivalent to the edge transition of the interrupt request signal.
//!     - When an SGI occurs in a multiprocessor implementation, the CPUID field in the Interrupt
//!       Acknowledge Register, GICC_IAR, or the Aliased Interrupt Acknowledge Register, GICC_AIAR,
//!       identifies the processor that requested the interrupt.
//!
//! # Architecture Specification - 2.2.1 Interrupt IDs
//!
//! Interrupts from sources are identified using ID numbers. Each CPU interface can see up to 1020
//! interrupts. The banking of SPIs and PPIs increases the total number of interrupts supported by
//! the Distributor.
//!
//! The GIC assigns interrupt ID numbers ID0-ID1019 as follows:
//!   - Interrupt numbers 32..1019 are used for SPIs.
//!   - Interrupt numbers 0..31 are used for interrupts that are private to a CPU interface. These
//!     interrupts are banked in the Distributor.
//!       - A banked interrupt is one where the Distributor can have multiple interrupts with the
//!         same ID. A banked interrupt is identified uniquely by its ID number and its associated
//!         CPU interface number. Of the banked interrupt IDs:
//!           - 00..15 SGIs
//!           - 16..31 PPIs
#![allow(dead_code)]
pub mod gicd;
pub mod gicr;
pub mod vgic;

use core::arch::asm;

use fdt::Fdt;
use spin::Once;

use crate::arch::aarch64::sysreg::{read_sysreg, smc_arg1, write_sysreg};
use crate::consts::MAX_CPU_NUM;
use crate::hypercall::{SGI_EVENT_ID, SGI_RESUME_ID};
use crate::percpu::check_events;

use self::gicd::enable_gic_are_ns;

//TODO: add Distributor init
pub fn irqchip_cpu_init() {
    //TODO: add Redistributor init
    let sdei_ver = unsafe { smc_arg1!(0xc4000020) }; //sdei_check();
    info!("gicv3 init: sdei version: {}", sdei_ver);

    //Identifier bits. Read-only and writes are ignored.
    //Priority bits. Read-only and writes are ignored.
    let ctlr = read_sysreg!(icc_ctlr_el1);
    debug!("ctlr: {:#x?}", ctlr);
    write_sysreg!(icc_ctlr_el1, 0x2); // ICC_EOIR1_EL1 provide priority drop functionality only. ICC_DIR_EL1 provides interrupt deactivation functionality.
    let ctlr2 = read_sysreg!(icc_ctlr_el1);
    debug!("ctlr2: {:#x?}", ctlr2);
    let pmr = read_sysreg!(icc_pmr_el1);
    write_sysreg!(icc_pmr_el1, 0xf0); // Interrupt Controller Interrupt Priority Mask Register
    let igrpen = read_sysreg!(icc_igrpen1_el1);
    write_sysreg!(icc_igrpen1_el1, 0x1); //group 1 irq
    debug!("ctlr: {:#x?}, pmr:{:#x?},igrpen{:#x?}", ctlr, pmr, igrpen);
    let _vtr = read_sysreg!(ich_vtr_el2);
    let vmcr = ((pmr & 0xff) << 24) | (1 << 1) | (1 << 9); //VPMR|VENG1|VEOIM
    write_sysreg!(ich_vmcr_el2, vmcr);
    write_sysreg!(ich_hcr_el2, 0x1); //enable virt cpu interface
}

fn gicv3_clear_pending_irqs() {
    let vtr = read_sysreg!(ich_vtr_el2) as usize;
    let lr_num: usize = (vtr & 0xf) + 1;
    for i in 0..lr_num {
        write_lr(i, 0) //clear lr
    }
    let num_priority_bits = (vtr >> 29) + 1;
    /* Clear active priority bits */
    if num_priority_bits >= 5 {
        write_sysreg!(ICH_AP1R0_EL2, 0); //Interrupt Controller Hyp Active Priorities Group 1 Register 0 No interrupt active
    }
    if num_priority_bits >= 6 {
        write_sysreg!(ICH_AP1R1_EL2, 0);
    }
    if num_priority_bits > 6 {
        write_sysreg!(ICH_AP1R2_EL2, 0);
        write_sysreg!(ICH_AP1R3_EL2, 0);
    }
}

pub fn gicv3_cpu_shutdown() {
    // unsafe {write_sysreg!(icc_sgi1r_el1, val);}
    // let intid = unsafe { read_sysreg!(icc_iar1_el1) } as u32;
    //arm_read_sysreg(ICC_CTLR_EL1, zone_icc_ctlr);
    info!("gicv3 shutdown!");
    let ctlr = read_sysreg!(icc_ctlr_el1);
    let pmr = read_sysreg!(icc_pmr_el1);
    let ich_hcr = read_sysreg!(ich_hcr_el2);
    debug!("ctlr: {:#x?}, pmr:{:#x?},ich_hcr{:#x?}", ctlr, pmr, ich_hcr);
    //TODO gicv3 reset
}

pub fn gicv3_handle_irq_el1() {
    if let Some(irq_id) = pending_irq() {
        // enum ipi_msg_type {
        //     IPI_WAKEUP,
        //     IPI_TIMER,
        //     IPI_RESCHEDULE,
        //     IPI_CALL_FUNC,
        //     IPI_CPU_STOP,
        //     IPI_IRQ_WORK,
        //     IPI_COMPLETION,
        //     /*
        //      * CPU_BACKTRACE is special and not included in NR_IPI
        //      * or tracable with trace_ipi_*
        //      */
        //     IPI_CPU_BACKTRACE,
        //     /*
        //      * SGI8-15 can be reserved by secure firmware, and thus may
        //      * not be usable by the kernel. Please keep the above limited
        //      * to at most 8 entries.
        //      */
        // };
        //SGI
        if irq_id < 16 {
            if irq_id < 8 {
                trace!("sgi get {},inject", irq_id);
                deactivate_irq(irq_id);
                inject_irq(irq_id);
            } else if irq_id == SGI_EVENT_ID as usize {
                info!("HV SGI EVENT {}", irq_id);
                check_events();
                deactivate_irq(irq_id);
            } else if irq_id == SGI_RESUME_ID as usize {
                info!("hv sgi got {}, resume", irq_id);
                // let cpu_data = unsafe { this_cpu_data() as &mut PerCpu };
                // cpu_data.suspend_cpu = false;
            } else {
                warn!("skip sgi {}", irq_id);
            }
        } else {
            trace!("spi/ppi get {}", irq_id);
            //inject phy irq
            if irq_id > 31 {
                trace!("*** get spi_irq id = {}", irq_id);
            }
            deactivate_irq(irq_id);
            inject_irq(irq_id);
        }
    }
    trace!("handle done")
}

fn pending_irq() -> Option<usize> {
    let iar = read_sysreg!(icc_iar1_el1) as usize;
    if iar >= 0x3fe {
        // spurious
        None
    } else {
        Some(iar as _)
    }
}

fn deactivate_irq(irq_id: usize) {
    write_sysreg!(icc_eoir1_el1, irq_id as u64);
    if irq_id < 16 {
        write_sysreg!(icc_dir_el1, irq_id as u64);
    }
    //write_sysreg!(icc_dir_el1, irq_id as usize);
}

fn read_lr(id: usize) -> u64 {
    let id = id as u64;
    match id {
        //TODO get lr size from gic reg
        0 => read_sysreg!(ich_lr0_el2),
        1 => read_sysreg!(ich_lr1_el2),
        2 => read_sysreg!(ich_lr2_el2),
        3 => read_sysreg!(ich_lr3_el2),
        4 => read_sysreg!(ich_lr4_el2),
        5 => read_sysreg!(ich_lr5_el2),
        6 => read_sysreg!(ich_lr6_el2),
        7 => read_sysreg!(ich_lr7_el2),
        8 => read_sysreg!(ich_lr8_el2),
        9 => read_sysreg!(ich_lr9_el2),
        10 => read_sysreg!(ich_lr10_el2),
        11 => read_sysreg!(ich_lr11_el2),
        12 => read_sysreg!(ich_lr12_el2),
        13 => read_sysreg!(ich_lr13_el2),
        14 => read_sysreg!(ich_lr14_el2),
        15 => read_sysreg!(ich_lr15_el2),
        _ => {
            error!("lr over");
            loop {}
        }
    }
}

fn write_lr(id: usize, val: u64) {
    let id = id as u64;
    match id {
        0 => write_sysreg!(ich_lr0_el2, val),
        1 => write_sysreg!(ich_lr1_el2, val),
        2 => write_sysreg!(ich_lr2_el2, val),
        3 => write_sysreg!(ich_lr3_el2, val),
        4 => write_sysreg!(ich_lr4_el2, val),
        5 => write_sysreg!(ich_lr5_el2, val),
        6 => write_sysreg!(ich_lr6_el2, val),
        7 => write_sysreg!(ich_lr7_el2, val),
        8 => write_sysreg!(ich_lr8_el2, val),
        9 => write_sysreg!(ich_lr9_el2, val),
        10 => write_sysreg!(ich_lr10_el2, val),
        11 => write_sysreg!(ich_lr11_el2, val),
        12 => write_sysreg!(ich_lr12_el2, val),
        13 => write_sysreg!(ich_lr13_el2, val),
        14 => write_sysreg!(ich_lr14_el2, val),
        15 => write_sysreg!(ich_lr15_el2, val),
        _ => {
            error!("lr over");
            loop {}
        }
    }
}

fn inject_irq(irq_id: usize) {
    // mask
    const LR_VIRTIRQ_MASK: usize = 0x3ff;
    // const LR_PHYSIRQ_MASK: usize = 0x3ff << 10;

    // const LR_PENDING_BIT: usize = 1 << 28;
    // const LR_HW_BIT: usize = 1 << 31;
    let elsr = read_sysreg!(ich_elrsr_el2);
    let vtr = read_sysreg!(ich_vtr_el2) as usize;
    let lr_num: usize = (vtr & 0xf) + 1;
    let mut lr_idx = -1 as isize;
    for i in 0..lr_num {
        if (1 << i) & elsr > 0 {
            if lr_idx == -1 {
                lr_idx = i as isize;
            }
            continue;
        }
        // overlap
        let _lr_val = read_lr(i) as usize;
        if (i & LR_VIRTIRQ_MASK) == irq_id {
            trace!("irq mask!{} {}", i, irq_id);
            return;
        }
    }
    debug!("To Inject IRQ {}, find lr {}", irq_id, lr_idx);

    if lr_idx == -1 {
        error!("full lr");
        loop {}
        // return;
    } else {
        // lr = irq_id;
        // /* Only group 1 interrupts */
        // lr |= ICH_LR_GROUP_BIT;
        // lr |= ICH_LR_PENDING;
        // if (!is_sgi(irq_id)) {
        //     lr |= ICH_LR_HW_BIT;
        //     lr |= (usize)irq_id << ICH_LR_PHYS_ID_SHIFT;
        // }
        let mut val = irq_id as usize; //v intid
        val |= 1 << 60; //group 1
        val |= 1 << 62; //state pending
        val |= 1 << 61; //map hardware
        val |= (irq_id as usize) << 32; //p intid
                                        //debug!("To write lr {} val {}", lr_idx, val);
        write_lr(lr_idx as usize, val as u64);
    }
}

pub static GIC: Once<Gic> = Once::new();
pub const PER_GICR_SIZE: usize = 0x20000;

#[derive(Debug)]
pub struct Gic {
    pub gicd_base: usize,
    pub gicr_base: usize,
    pub gicd_size: usize,
    pub gicr_size: usize,
}

impl Gic {
    pub fn new(fdt: &Fdt) -> Self {
        let gic_info = fdt
            .find_node("/gic")
            .unwrap_or_else(|| fdt.find_node("/intc").unwrap());
        let mut reg_iter = gic_info.reg().unwrap();

        let first_reg = reg_iter.next().unwrap();
        let second_reg = reg_iter.next().unwrap();

        Self {
            gicd_base: first_reg.starting_address as usize,
            gicr_base: second_reg.starting_address as usize,
            gicd_size: first_reg.size.unwrap(),
            gicr_size: second_reg.size.unwrap(),
        }
    }
}

pub fn host_gicd_base() -> usize {
    GIC.get().unwrap().gicd_base
}

pub fn host_gicr_base(id: usize) -> usize {
    assert!(id < MAX_CPU_NUM);
    GIC.get().unwrap().gicr_base + id * PER_GICR_SIZE
}

pub fn host_gicd_size() -> usize {
    GIC.get().unwrap().gicd_size
}

pub fn host_gicr_size() -> usize {
    GIC.get().unwrap().gicr_size
}

pub fn is_spi(irqn: u32) -> bool {
    irqn > 31 && irqn < 1020
}

pub fn enable_irqs() {
    unsafe { asm!("msr daifclr, #0xf") };
}

pub fn disable_irqs() {
    unsafe { asm!("msr daifset, #0xf") };
}

pub fn init_early(host_fdt: &Fdt) {
    GIC.call_once(|| Gic::new(host_fdt));
    debug!("gic = {:#x?}", GIC.get().unwrap());
}

pub fn init_late() {
    enable_gic_are_ns();
}
