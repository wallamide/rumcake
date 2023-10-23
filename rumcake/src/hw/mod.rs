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

use defmt::{assert, info};
use ekv::config;
use embedded_storage::nor_flash::NorFlash;

use self::mcu::setup_internal_flash;

extern "C" {
    // Comes from memory.x
    pub static __config_start: u32;
    pub static __config_end: u32;
}

#[repr(C, align(4))]
struct AlignedBuf([u8; config::PAGE_SIZE]);

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
        (config_end - config_start) % ekv::config::PAGE_SIZE == 0,
        "Config partition size must be a multiple of the page size."
    );
    assert!(
        config_start % ekv::config::PAGE_SIZE == 0,
        "Config partition must start on an address that is a multiple of the page size."
    );

    FlashDevice {
        flash: driver,
        start: config_start,
        end: config_end,
    }
}

/// Storage driver intended for internal flash
pub struct FlashDevice<F: NorFlash> {
    flash: F,
    start: usize,
    end: usize,
}

impl<F: NorFlash> ekv::flash::Flash for FlashDevice<F> {
    type Error = F::Error;

    fn page_count(&self) -> usize {
        (self.end - self.start) / ekv::config::PAGE_SIZE
    }

    async fn erase(&mut self, page_id: ekv::flash::PageID) -> Result<(), Self::Error> {
        let page_index = page_id.index();
        let start = self.start + page_index * config::PAGE_SIZE;
        let end = self.start + page_index * config::PAGE_SIZE + config::PAGE_SIZE;
        info!(
            "[STORAGE] Erasing page {}. Start addr = {}, End addr = {}, PS = {}",
            page_index,
            start,
            end,
            config::PAGE_SIZE
        );
        self.flash.erase((start) as u32, (end) as u32)
    }

    async fn read(
        &mut self,
        page_id: ekv::flash::PageID,
        offset: usize,
        data: &mut [u8],
    ) -> Result<(), Self::Error> {
        let page_index = page_id.index();
        info!(
            "[STORAGE] Reading from page {}, offset {}",
            page_index, offset
        );
        let address = self.start + page_index * config::PAGE_SIZE + offset;
        let mut buf = AlignedBuf([0; config::PAGE_SIZE]);
        self.flash.read(address as u32, &mut buf.0[..data.len()])?;
        data.copy_from_slice(&buf.0[..data.len()]);
        Ok(())
    }

    async fn write(
        &mut self,
        page_id: ekv::flash::PageID,
        offset: usize,
        data: &[u8],
    ) -> Result<(), Self::Error> {
        let page_index = page_id.index();
        info!(
            "[STORAGE] Writing to page {}, offset {}, data: {}",
            page_index, offset, data
        );
        let address = self.start + page_index * config::PAGE_SIZE + offset;
        let mut buf = AlignedBuf([0; config::PAGE_SIZE]);
        buf.0[..data.len()].copy_from_slice(data);
        self.flash.write(address as u32, &buf.0[..data.len()])
    }
}
