#[cfg(all(not(feature = "stm32"), not(feature = "nrf")))]
compile_error!("Please enable the appropriate feature flag for the chip you're using.");

#[cfg(all(feature = "stm32", feature = "nrf"))]
compile_error!("Please enable only one chip feature flag.");

#[cfg_attr(feature = "stm32", path = "mcu/stm32.rs")]
#[cfg_attr(feature = "nrf", path = "mcu/nrf.rs")]
pub mod mcu;

use crate::State;
pub static BATTERY_LEVEL_STATE: State<u8> = State::new(
    100,
    &[
        #[cfg(feature = "display")]
        &crate::display::BATTERY_LEVEL_LISTENER,
        #[cfg(feature = "bluetooth")]
        &crate::bluetooth::BATTERY_LEVEL_LISTENER,
    ],
);

use core::cell::RefCell;
use defmt::{assert, debug, error};
use embedded_storage::nor_flash::NorFlash;
use tickv::FlashController;

extern "C" {
    // Comes from memory.x
    pub static __config_start: u32;
    pub static __config_end: u32;
}

pub fn setup_storage_driver<F: NorFlash>(
    driver: F,
    config_start: usize,
    config_end: usize,
) -> FlashDevice<F> {
    // Check config partition before moving on
    assert!(
        config_start < config_end,
        "Config end address must be greater than the start address."
    );
    assert!(
        (config_end - config_start) % F::ERASE_SIZE == 0,
        "Config partition size must be a multiple of the page size."
    );
    assert!(
        config_start % F::ERASE_SIZE == 0,
        "Config partition must start on an address that is a multiple of the page size."
    );

    FlashDevice {
        flash: RefCell::new(driver),
        start: config_start,
        end: config_end,
    }
}

pub struct FlashDevice<F: NorFlash> {
    pub flash: RefCell<F>,
    pub start: usize,
    pub end: usize,
}

impl<F: NorFlash> FlashController<{ F::ERASE_SIZE }> for FlashDevice<F> {
    fn read_region(
        &self,
        region_number: usize,
        offset: usize,
        buf: &mut [u8; F::ERASE_SIZE],
    ) -> Result<(), tickv::ErrorCode> {
        let address = self.start + region_number * F::ERASE_SIZE + offset;

        debug!(
            "[STORAGE_DRIVER] Reading {} bytes from config page {}, offset {} (address = {})",
            buf.len(),
            region_number,
            offset,
            address
        );

        if let Err(err) = self.flash.borrow_mut().read(address as u32, buf) {
            error!(
                "[STORAGE_DRIVER] Failed to read: {}",
                defmt::Debug2Format(&err)
            );
            return Err(tickv::ErrorCode::ReadFail);
        };

        Ok(())
    }

    fn write(&self, address: usize, buf: &[u8]) -> Result<(), tickv::ErrorCode> {
        debug!(
            "[STORAGE_DRIVER] Writing to address {} (config page {}, offset {}). data: {}",
            address,
            address / F::ERASE_SIZE,
            address % F::ERASE_SIZE,
            buf
        );

        let mut write_buf = [0xFF; F::ERASE_SIZE];

        let mut flash = self.flash.borrow_mut();

        if let Err(err) = flash.read(
            (self.start + address - address % F::ERASE_SIZE) as u32,
            &mut write_buf,
        ) {
            error!(
                "[STORAGE_DRIVER] Failed to read page data before writing: {}",
                defmt::Debug2Format(&err),
            );
            return Err(tickv::ErrorCode::WriteFail);
        };

        if let Err(err) = flash.erase(
            (self.start + address - address % F::ERASE_SIZE) as u32,
            (self.start + address - address % F::ERASE_SIZE + F::ERASE_SIZE) as u32,
        ) {
            error!(
                "[STORAGE_DRIVER] Failed to erase page before writing: {}",
                defmt::Debug2Format(&err),
            );
            return Err(tickv::ErrorCode::WriteFail);
        };

        let offset = address % F::ERASE_SIZE;
        write_buf[offset..(offset + buf.len())].copy_from_slice(buf);

        if let Err(err) = flash.write(
            (self.start + ((address / F::ERASE_SIZE) * F::ERASE_SIZE)) as u32,
            &write_buf,
        ) {
            error!(
                "[STORAGE_DRIVER] Failed to write: {}",
                defmt::Debug2Format(&err),
            );
            return Err(tickv::ErrorCode::WriteFail);
        }

        Ok(())
    }

    fn erase_region(&self, region_number: usize) -> Result<(), tickv::ErrorCode> {
        let start = self.start + region_number * F::ERASE_SIZE;
        let end = self.start + region_number * F::ERASE_SIZE + F::ERASE_SIZE;

        debug!(
            "[STORAGE_DRIVER] Erasing config page {} (start addr = {}, end addr = {}).",
            region_number, start, end
        );

        if let Err(err) = self.flash.borrow_mut().erase(start as u32, end as u32) {
            error!(
                "[STORAGE_DRIVER] Failed to erase: {}",
                defmt::Debug2Format(&err)
            );
            return Err(tickv::ErrorCode::EraseFail);
        }

        Ok(())
    }
}
