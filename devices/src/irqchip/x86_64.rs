// Copyright 2020 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use base::Result;
use hypervisor::{IoapicState, LapicState, PicSelect, PicState, PitState};

use crate::IrqChip;

pub trait IrqChipX86_64: IrqChip {
    // Clones this trait as a `Box` version of itself.
    fn try_box_clone(&self) -> Result<Box<dyn IrqChipX86_64>>;

    // Get this as the super-trait IrqChip.
    fn as_irq_chip(&self) -> &dyn IrqChip;

    // Get this as the mutable super-trait IrqChip.
    fn as_irq_chip_mut(&mut self) -> &mut dyn IrqChip;

    /// Get the current state of the PIC
    fn get_pic_state(&self, select: PicSelect) -> Result<PicState>;

    /// Set the current state of the PIC
    fn set_pic_state(&mut self, select: PicSelect, state: &PicState) -> Result<()>;

    /// Get the current state of the IOAPIC
    fn get_ioapic_state(&self) -> Result<IoapicState>;

    /// Set the current state of the IOAPIC
    fn set_ioapic_state(&mut self, state: &IoapicState) -> Result<()>;

    /// Get the current state of the specified VCPU's local APIC
    fn get_lapic_state(&self, vcpu_id: usize) -> Result<LapicState>;

    /// Set the current state of the specified VCPU's local APIC
    fn set_lapic_state(&mut self, vcpu_id: usize, state: &LapicState) -> Result<()>;

    /// Retrieves the state of the PIT.
    fn get_pit(&self) -> Result<PitState>;

    /// Sets the state of the PIT.
    fn set_pit(&mut self, state: &PitState) -> Result<()>;

    /// Returns true if the PIT uses port 0x61 for the PC speaker, false if 0x61 is unused.
    fn pit_uses_speaker_port(&self) -> bool;
}

#[cfg(test)]
/// This module contains tests that apply to any implementations of IrqChipX86_64
pub(super) mod tests {
    use super::*;
    use hypervisor::{IrqRoute, IrqSource, IrqSourceChip};

    pub fn test_get_pic(mut chip: impl IrqChipX86_64) {
        let state = chip
            .get_pic_state(PicSelect::Primary)
            .expect("could not get pic state");

        // Default is that no irq lines are asserted
        assert_eq!(state.irr, 0);

        // Assert Irq Line 0
        chip.service_irq(0, true).expect("could not service irq");

        let state = chip
            .get_pic_state(PicSelect::Primary)
            .expect("could not get pic state");

        // Bit 0 should now be 1
        assert_eq!(state.irr, 1);
    }

    pub fn test_set_pic(mut chip: impl IrqChipX86_64) {
        let mut state = chip
            .get_pic_state(PicSelect::Primary)
            .expect("could not get pic state");

        // set bits 0 and 1
        state.irr = 3;

        chip.set_pic_state(PicSelect::Primary, &state)
            .expect("could not set the pic state");

        let state = chip
            .get_pic_state(PicSelect::Primary)
            .expect("could not get pic state");

        // Bits 1 and 0 should now be 1
        assert_eq!(state.irr, 3);
    }

    pub fn test_get_ioapic(mut chip: impl IrqChipX86_64) {
        let state = chip.get_ioapic_state().expect("could not get ioapic state");

        // Default is that no irq lines are asserted
        assert_eq!(state.current_interrupt_level_bitmap, 0);

        // Default routing entries has routes 0..24 routed to vectors 0..24
        for i in 0..24 {
            // when the ioapic is reset by kvm, it defaults to all zeroes except the
            // interrupt mask is set to 1, which is bit 16
            assert_eq!(state.redirect_table[i].get(0, 64), 1 << 16);
        }

        // Assert Irq Line 1
        chip.service_irq(1, true).expect("could not set irq line");

        let state = chip.get_ioapic_state().expect("could not get ioapic state");

        // Bit 1 should now be 1
        assert_eq!(state.current_interrupt_level_bitmap, 2);
    }

    pub fn test_set_ioapic(mut chip: impl IrqChipX86_64) {
        let mut state = chip.get_ioapic_state().expect("could not get ioapic state");

        // set a vector in the redirect table
        state.redirect_table[2].set_vector(15);
        // set the irq line status on that entry
        state.current_interrupt_level_bitmap = 4;

        chip.set_ioapic_state(&state)
            .expect("could not set the ioapic state");

        let state = chip.get_ioapic_state().expect("could not get ioapic state");

        // verify that get_ioapic_state returns what we set
        assert_eq!(state.redirect_table[2].get_vector(), 15);
        assert_eq!(state.current_interrupt_level_bitmap, 4);
    }

    pub fn test_get_pit(chip: impl IrqChipX86_64) {
        let state = chip.get_pit().expect("failed to get pit state");

        assert_eq!(state.flags, 0);
        // assert reset state of pit
        for i in 0..3 {
            // initial count of 0 sets it to 0x10000;
            assert_eq!(state.channels[i].count, 0x10000);
        }
    }

    pub fn test_set_pit(mut chip: impl IrqChipX86_64) {
        let mut state = chip.get_pit().expect("failed to get pit state");

        // set some values
        state.channels[0].count = 500;
        state.channels[0].mode = 1;

        // Setting the pit should initialize the one-shot timer
        chip.set_pit(&state).expect("failed to set pit state");

        let state = chip.get_pit().expect("failed to get pit state");

        // check the values we set
        assert_eq!(state.channels[0].count, 500);
        assert_eq!(state.channels[0].mode, 1);
    }

    pub fn test_get_lapic(chip: impl IrqChipX86_64) {
        let state = chip.get_lapic_state(0).expect("failed to get lapic state");

        // Checking some APIC reg defaults for KVM:
        // DFR default is 0xffffffff
        assert_eq!(state.regs[0xe], 0xffffffff);
        // SPIV default is 0xff
        assert_eq!(state.regs[0xf], 0xff);
    }

    pub fn test_set_lapic(mut chip: impl IrqChipX86_64) {
        // Get default state
        let mut state = chip.get_lapic_state(0).expect("failed to get lapic state");

        // ESR should start out as 0
        assert_eq!(state.regs[8], 0);
        // Set a value in the ESR
        state.regs[8] = 1 << 8;
        chip.set_lapic_state(0, &state)
            .expect("failed to set lapic state");

        // check that new ESR value stuck
        let state = chip.get_lapic_state(0).expect("failed to get lapic state");
        assert_eq!(state.regs[8], 1 << 8);
    }

    /// Helper function for checking the pic interrupt status
    fn check_pic_interrupts(chip: &impl IrqChipX86_64, select: PicSelect, value: u8) {
        let state = chip
            .get_pic_state(select)
            .expect("could not get ioapic state");

        assert_eq!(state.irr, value);
    }

    /// Helper function for checking the ioapic interrupt status
    fn check_ioapic_interrupts(chip: &impl IrqChipX86_64, value: u32) {
        let state = chip.get_ioapic_state().expect("could not get ioapic state");

        // since the irq route goes nowhere the bitmap should still be 0
        assert_eq!(state.current_interrupt_level_bitmap, value);
    }

    pub fn test_route_irq(mut chip: impl IrqChipX86_64) {
        // clear out irq routes
        chip.set_irq_routes(&[])
            .expect("failed to set empty irq routes");
        // assert Irq Line 1
        chip.service_irq(1, true).expect("could not set irq line");

        // no pic or ioapic interrupts should be asserted
        check_pic_interrupts(&chip, PicSelect::Primary, 0);
        check_ioapic_interrupts(&chip, 0);

        // now we route gsi 1 to pin 3 of the ioapic and pin 6 of the primary pic
        chip.route_irq(IrqRoute {
            gsi: 1,
            source: IrqSource::Irqchip {
                chip: IrqSourceChip::Ioapic,
                pin: 3,
            },
        })
        .expect("failed to assert irq route");
        // re-assert Irq Line 1
        chip.service_irq(1, true).expect("could not set irq line");

        // no pic line should be asserted, ioapic pin 3 should be asserted
        check_pic_interrupts(&chip, PicSelect::Primary, 0);
        check_ioapic_interrupts(&chip, 1 << 3);

        // de-assert Irq Line 1
        chip.service_irq(1, false).expect("could not set irq line");

        // no pic or ioapic interrupts should be asserted
        check_pic_interrupts(&chip, PicSelect::Primary, 0);
        check_ioapic_interrupts(&chip, 0);

        // add pic route
        chip.route_irq(IrqRoute {
            gsi: 2,
            source: IrqSource::Irqchip {
                chip: IrqSourceChip::PicPrimary,
                pin: 6,
            },
        })
        .expect("failed to route irq");

        // re-assert Irq Line 1, it should still affect only the ioapic
        chip.service_irq(1, true).expect("could not set irq line");

        // no pic line should be asserted, ioapic pin 3 should be asserted
        check_pic_interrupts(&chip, PicSelect::Primary, 0);
        check_ioapic_interrupts(&chip, 1 << 3);

        // assert Irq Line 2
        chip.service_irq(2, true).expect("could not set irq line");

        // pic pin 6 should be asserted, ioapic pin 3 should be asserted
        check_pic_interrupts(&chip, PicSelect::Primary, 1 << 6);
        check_ioapic_interrupts(&chip, 1 << 3);
    }
}
