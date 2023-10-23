use core::any::TypeId;
use core::hash::{Hash, Hasher, SipHasher};
use core::mem::size_of;

use defmt::{error, info, panic, warn, Debug2Format};
use embassy_futures::{join, select};
use embassy_sync::blocking_mutex::raw::{NoopRawMutex, ThreadModeRawMutex};
use embassy_sync::channel::{Channel, Sender};
use embassy_sync::signal::Signal;
use embedded_storage::nor_flash::NorFlash;
use num_derive::FromPrimitive;
use postcard::experimental::max_size::MaxSize;
use serde::de::DeserializeOwned;
use serde::Serialize;
use tickv::{TicKV, MAIN_KEY};

use crate::hw::FlashDevice;
use crate::keyboard::Keyboard;

fn get_hashed_key(key: &[u8]) -> u64 {
    let mut hasher = SipHasher::new();
    key.hash(&mut hasher);
    hasher.finish()
}

pub enum StorageRequest<T> {
    Read,
    Write(T),
    Delete,
}

pub enum StorageResponse<T> {
    Read(Result<T, ()>),
    Write(Result<(), ()>),
    Delete(Result<(), ()>),
}

/// Keys for data to be stored in the database. The order of existing keys should not change.
#[derive(Debug, FromPrimitive)]
#[repr(u8)]
pub(crate) enum StorageKey {
    BacklightConfig,
    UnderglowConfig,
}

#[repr(u8)]
enum StorageKeyType {
    Data,
    Metadata,
}

pub struct StorageService<
    T: 'static + DeserializeOwned + Serialize + MaxSize,
    const K: u8,
    const N: usize,
> {
    requests: Channel<
        ThreadModeRawMutex,
        (
            StorageRequest<T>,
            Sender<'static, ThreadModeRawMutex, StorageResponse<T>, N>,
        ),
        N,
    >,
    pub(crate) signal: Signal<ThreadModeRawMutex, ()>,
}

impl<T: Clone + Send + DeserializeOwned + Serialize + MaxSize, const K: u8, const N: usize>
    StorageService<T, K, N>
{
    pub const fn new() -> Self {
        StorageService {
            requests: Channel::new(),
            signal: Signal::new(),
        }
    }

    pub const fn client(&'static self) -> StorageClient<T, K, N> {
        StorageClient {
            service: self,
            response_channel: Channel::new(),
        }
    }

    pub async fn initialize<F: NorFlash>(
        &self,
        database: &TicKV<'_, FlashDevice<F>, { F::ERASE_SIZE }>,
    ) -> Result<(), ()> {
        let cur_type_id: [u8; size_of::<TypeId>()] =
            unsafe { core::mem::transmute(TypeId::of::<T>()) };

        // Verify if the underlying data type has changed since last boot
        let mut stored_type_id = [0; size_of::<TypeId>()];
        let will_reset = match database.get_key(
            get_hashed_key(&[K, StorageKeyType::Metadata as u8]),
            &mut stored_type_id,
        ) {
            Ok(_) => {
                let changed = cur_type_id != stored_type_id;
                if changed {
                    warn!(
                        "[STORAGE] Metadata for {} has changed.",
                        Debug2Format(&<StorageKey as num::FromPrimitive>::from_u8(K).unwrap()),
                    );
                }
                changed
            }
            Err(error) => {
                warn!(
                    "[STORAGE] Could not read metadata for {}: {}",
                    Debug2Format(&<StorageKey as num::FromPrimitive>::from_u8(K).unwrap()),
                    Debug2Format(&error)
                );
                true
            }
        };

        // If the data type has changed, remove the old data from storage, update the metadata
        if will_reset {
            warn!(
                "[STORAGE] Deleting old data and updating stored metadata for {}.",
                Debug2Format(&<StorageKey as num::FromPrimitive>::from_u8(K).unwrap()),
            );

            // Invalidate old data
            let _ = database.invalidate_key(get_hashed_key(&[K, StorageKeyType::Data as u8]));
            let _ = database.invalidate_key(get_hashed_key(&[K, StorageKeyType::Metadata as u8]));
            database.garbage_collect().unwrap();

            // Add new metadata
            database
                .append_key(
                    get_hashed_key(&[K, StorageKeyType::Metadata as u8]),
                    &cur_type_id,
                )
                .unwrap();
        }

        Ok(())
    }

    pub async fn handle_requests<F: NorFlash>(
        &self,
        database: &TicKV<'_, FlashDevice<F>, { F::ERASE_SIZE }>,
    ) where
        [(); T::POSTCARD_MAX_SIZE]:,
    {
        while let Ok((req, response_channel)) = self.requests.try_receive() {
            match req {
                StorageRequest::Read => {
                    info!(
                        "[STORAGE] Reading {} data.",
                        Debug2Format(&<StorageKey as num::FromPrimitive>::from_u8(K).unwrap()),
                    );

                    let result = {
                        let mut buf = [0; T::POSTCARD_MAX_SIZE];
                        database
                            .get_key(get_hashed_key(&[K, StorageKeyType::Data as u8]), &mut buf)
                            .map_err(|error| {
                                error!(
                                    "[STORAGE] Read error for {}: {}",
                                    Debug2Format(
                                        &<StorageKey as num::FromPrimitive>::from_u8(K).unwrap()
                                    ),
                                    Debug2Format(&error)
                                );
                            })
                            .and_then(|_code| {
                                postcard::from_bytes(&buf).map_err(|error| {
                                    error!(
                                        "[STORAGE] Deserialization error while reading {}: {}",
                                        Debug2Format(
                                            &<StorageKey as num::FromPrimitive>::from_u8(K)
                                                .unwrap()
                                        ),
                                        Debug2Format(&error)
                                    );
                                })
                            })
                    };

                    response_channel.send(StorageResponse::Read(result)).await;
                }
                StorageRequest::Write(data) => {
                    info!(
                        "[STORAGE] Writing new {} data.",
                        Debug2Format(&<StorageKey as num::FromPrimitive>::from_u8(K).unwrap()),
                    );

                    let result = {
                        match postcard::to_slice(&data, &mut [0; T::POSTCARD_MAX_SIZE]) {
                            Ok(serialized) => {
                                let _ = database.invalidate_key(get_hashed_key(&[
                                    K,
                                    StorageKeyType::Data as u8,
                                ]));
                                database.garbage_collect().unwrap();
                                database
                                    .append_key(
                                        get_hashed_key(&[K, StorageKeyType::Data as u8]),
                                        serialized,
                                    )
                                    .map_err(|error| {
                                        error!(
                                            "[STORAGE] Write error for {}: {}",
                                            Debug2Format(
                                                &<StorageKey as num::FromPrimitive>::from_u8(K)
                                                    .unwrap()
                                            ),
                                            Debug2Format(&error)
                                        );
                                    })
                            }
                            Err(error) => {
                                error!(
                                    "[STORAGE] Serialization error while writing {}: {}",
                                    Debug2Format(
                                        &<StorageKey as num::FromPrimitive>::from_u8(K).unwrap()
                                    ),
                                    Debug2Format(&error)
                                );
                                Err(())
                            }
                        }
                    };

                    response_channel
                        .send(StorageResponse::Write(result.map(|_code| {})))
                        .await;
                }
                StorageRequest::Delete => {
                    info!(
                        "[STORAGE] Deleting {} data.",
                        Debug2Format(&<StorageKey as num::FromPrimitive>::from_u8(K).unwrap()),
                    );

                    let result = database
                        .invalidate_key(get_hashed_key(&[K, StorageKeyType::Data as u8]))
                        .map_err(|error| {
                            error!("[STORAGE] Delete error: {}", Debug2Format(&error));
                        });
                    database.garbage_collect().unwrap();
                    response_channel
                        .send(StorageResponse::Delete(result.map(|_code| {})))
                        .await;
                }
            };
        }
    }
}

pub struct StorageClient<
    T: 'static + DeserializeOwned + Serialize + MaxSize,
    const K: u8,
    const N: usize,
> {
    service: &'static StorageService<T, K, N>,
    response_channel: Channel<ThreadModeRawMutex, StorageResponse<T>, N>,
}

impl<T: 'static + DeserializeOwned + Serialize + MaxSize, const K: u8, const N: usize>
    StorageClient<T, K, N>
{
    pub async fn request(&'static self, req: StorageRequest<T>) -> StorageResponse<T> {
        self.service
            .requests
            .send((req, self.response_channel.sender()))
            .await;
        self.service.signal.signal(());
        self.response_channel.receive().await
    }
}

pub trait KeyboardWithEEPROM: Keyboard {
    // Probably not using these.
    const EECONFIG_KB_DATA_SIZE: usize = 0; // This is the default if it is not set in QMK
    const EECONFIG_USER_DATA_SIZE: usize = 0; // This is the default if it is not set in QMK

    // While most of these are not implemented in this firmware, our EECONFIG addresses will follow the same structure that QMK uses.
    const EECONFIG_MAGIC_ADDR: u16 = 0;
    const EECONFIG_DEBUG_ADDR: u8 = 2;
    const EECONFIG_DEFAULT_LAYER_ADDR: u8 = 3;
    const EECONFIG_KEYMAP_ADDR: u16 = 4;
    const EECONFIG_BACKLIGHT_ADDR: u8 = 6;
    const EECONFIG_AUDIO_ADDR: u8 = 7;
    const EECONFIG_RGBLIGHT_ADDR: u32 = 8;
    const EECONFIG_UNICODEMODE_ADDR: u8 = 12;
    const EECONFIG_STENOMODE_ADDR: u8 = 13;
    const EECONFIG_HANDEDNESS_ADDR: u8 = 14;
    const EECONFIG_KEYBOARD_ADDR: u32 = 15;
    const EECONFIG_USER_ADDR: u32 = 19;
    const EECONFIG_VELOCIKEY_ADDR: u8 = 23;
    const EECONFIG_LED_MATRIX_ADDR: u32 = 24;
    const EECONFIG_RGB_MATRIX_ADDR: u64 = 24;
    const EECONFIG_HAPTIC_ADDR: u32 = 32;
    const EECONFIG_RGBLIGHT_EXTENDED_ADDR: u8 = 36;

    // Note: this is just the *base* size to use the features above.
    // VIA will use more EECONFIG space starting at the address below (address 37) and beyond.
    const EECONFIG_BASE_SIZE: usize = 37;
    const EECONFIG_SIZE: usize =
        Self::EECONFIG_BASE_SIZE + Self::EECONFIG_KB_DATA_SIZE + Self::EECONFIG_USER_DATA_SIZE;

    // Note: QMK uses an algorithm to emulate EEPROM in STM32 chips by using their flash peripherals
    const EEPROM_TOTAL_BYTE_COUNT: usize = Self::EECONFIG_SIZE + 3;
}

static EMPTY_SIGNAL: Signal<ThreadModeRawMutex, ()> = Signal::new();

#[rumcake_macros::task]
pub async fn storage_task<F: NorFlash>(driver: FlashDevice<F>)
where
    [(); F::ERASE_SIZE]:,
{
    let mut read_buf = [0; F::ERASE_SIZE];
    let flash_size = driver.end - driver.start;
    let database = tickv::TicKV::new(driver, &mut read_buf, flash_size);

    // Initialize the database, formatting if needed
    database.initialise(get_hashed_key(MAIN_KEY)).unwrap();

    // Initialize all services
    #[cfg(feature = "backlight")]
    crate::backlight::BACKLIGHT_CONFIG_STORAGE_SERVICE
        .initialize(&database)
        .await
        .unwrap();
    #[cfg(feature = "underglow")]
    crate::underglow::UNDERGLOW_CONFIG_STORAGE_SERVICE
        .initialize(&database)
        .await
        .unwrap();

    loop {
        let ((), index) = select::select_array([
            #[cfg(feature = "backlight")]
            crate::backlight::BACKLIGHT_CONFIG_STORAGE_SERVICE
                .signal
                .wait(),
            #[cfg(not(feature = "backlight"))]
            EMPTY_SIGNAL.wait(),
            #[cfg(feature = "underglow")]
            crate::underglow::UNDERGLOW_CONFIG_STORAGE_SERVICE
                .signal
                .wait(),
            #[cfg(not(feature = "underglow"))]
            EMPTY_SIGNAL.wait(),
        ])
        .await;

        match index {
            0 => {
                #[cfg(feature = "backlight")]
                crate::backlight::BACKLIGHT_CONFIG_STORAGE_SERVICE
                    .handle_requests(&database)
                    .await;
            }
            1 => {
                #[cfg(feature = "underglow")]
                crate::underglow::UNDERGLOW_CONFIG_STORAGE_SERVICE
                    .handle_requests(&database)
                    .await;
            }
            _ => {}
        };
    }
}
