use core::any::TypeId;
use core::hash::{Hash, Hasher, SipHasher};
use core::mem::size_of;

use defmt::{error, info, warn, Debug2Format};
use embassy_futures::select;
use embassy_sync::blocking_mutex::raw::ThreadModeRawMutex;
use embassy_sync::channel::{Channel, Sender};
use embassy_sync::signal::Signal;
use embedded_storage_async::nor_flash::NorFlash;
use num_derive::FromPrimitive;
use postcard::experimental::max_size::MaxSize;
use serde::de::DeserializeOwned;
use serde::Serialize;
use tickv::success_codes::SuccessCode;
use tickv::{AsyncTicKV, ErrorCode, MAIN_KEY};

use crate::hw::{FlashDevice, PendingOperation};
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
> where
    [(); T::POSTCARD_MAX_SIZE]:,
{
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

pub struct StorageServiceState<T: 'static + DeserializeOwned + Serialize + MaxSize>
where
    [(); T::POSTCARD_MAX_SIZE]:,
{
    stored_type_id: [u8; size_of::<TypeId>()],
    cur_type_id: [u8; size_of::<TypeId>()],
    stored_value: [u8; T::POSTCARD_MAX_SIZE],
    new_value: [u8; T::POSTCARD_MAX_SIZE],
}

impl<T: 'static + DeserializeOwned + Serialize + MaxSize> StorageServiceState<T>
where
    [(); T::POSTCARD_MAX_SIZE]:,
{
    pub const fn new() -> Self {
        Self {
            stored_type_id: [0; size_of::<TypeId>()],
            cur_type_id: [0; size_of::<TypeId>()],
            stored_value: [0; T::POSTCARD_MAX_SIZE],
            new_value: [0; T::POSTCARD_MAX_SIZE],
        }
    }
}

impl<T: Clone + Send + DeserializeOwned + Serialize + MaxSize, const K: u8, const N: usize>
    StorageService<T, K, N>
where
    [(); T::POSTCARD_MAX_SIZE]:,
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
        &'static self,
        database: &mut AsyncTicKV<'_, FlashDevice<F>, { F::ERASE_SIZE }>,
        state: &'static mut StorageServiceState<T>,
    ) -> Result<(), ()> {
        let stored = &mut state.stored_type_id;
        let cur = &mut state.cur_type_id;

        let current: [u8; size_of::<TypeId>()] = unsafe { core::mem::transmute(TypeId::of::<T>()) };
        cur.copy_from_slice(&current);

        // Verify if the underlying data type has changed since last boot
        let will_reset = match get_key(database, &[K, StorageKeyType::Metadata as u8], stored).await
        {
            (Ok(_), Some(type_id_buf), _len) => {
                let changed = *cur != *type_id_buf;
                if changed {
                    warn!(
                        "[STORAGE] Metadata for {} has changed.",
                        Debug2Format(&<StorageKey as num::FromPrimitive>::from_u8(K).unwrap()),
                    );
                }
                changed
            }
            (Err(error), _buf, _len) => {
                warn!(
                    "[STORAGE] Could not read metadata for {}: {}",
                    Debug2Format(&<StorageKey as num::FromPrimitive>::from_u8(K).unwrap()),
                    Debug2Format(&error)
                );
                true
            }
            _ => unreachable!(),
        };

        // If the data type has changed, remove the old data from storage, update the metadata
        if will_reset {
            warn!(
                "[STORAGE] Deleting old data and updating stored metadata for {}.",
                Debug2Format(&<StorageKey as num::FromPrimitive>::from_u8(K).unwrap()),
            );

            // Invalidate old data
            let _ = invalidate_key(database, &[K, StorageKeyType::Data as u8]).await;
            let _ = invalidate_key(database, &[K, StorageKeyType::Metadata as u8]).await;
            garbage_collect(database).await.0.unwrap();

            // Add new metadata
            let length = cur.len();
            append_key(database, &[K, StorageKeyType::Metadata as u8], cur, length)
                .await
                .0
                .unwrap();
        }

        Ok(())
    }

    pub async fn handle_request<F: NorFlash>(
        &'static self,
        database: &mut AsyncTicKV<'_, FlashDevice<F>, { F::ERASE_SIZE }>,
        state: &'static mut StorageServiceState<T>,
        req: StorageRequest<T>,
        response_channel: Sender<'static, ThreadModeRawMutex, StorageResponse<T>, N>,
    ) where
        [(); T::POSTCARD_MAX_SIZE]:,
    {
        let stored_value_buf = &mut state.stored_value;
        let new_value_buf = &mut state.new_value;
        match req {
            StorageRequest::Read => {
                info!(
                    "[STORAGE] Reading {} data.",
                    Debug2Format(&<StorageKey as num::FromPrimitive>::from_u8(K).unwrap()),
                );

                let result = {
                    let (result, buf, _len) =
                        get_key(database, &[K, StorageKeyType::Data as u8], stored_value_buf).await;

                    result
                        .map_err(|error| {
                            error!(
                                "[STORAGE] Read error for {}: {}",
                                Debug2Format(
                                    &<StorageKey as num::FromPrimitive>::from_u8(K).unwrap()
                                ),
                                Debug2Format(&error)
                            );
                        })
                        .and_then(|_code| match buf {
                            Some(buf) => postcard::from_bytes(buf).map_err(|error| {
                                error!(
                                    "[STORAGE] Deserialization error while reading {}: {}",
                                    Debug2Format(
                                        &<StorageKey as num::FromPrimitive>::from_u8(K).unwrap()
                                    ),
                                    Debug2Format(&error)
                                );
                            }),
                            None => unreachable!(),
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
                    match postcard::to_slice(&data, new_value_buf) {
                        Ok(serialized) => {
                            let _ =
                                invalidate_key(database, &[K, StorageKeyType::Data as u8]).await;
                            garbage_collect(database).await.0.unwrap();
                            append_key(
                                database,
                                &[K, StorageKeyType::Data as u8],
                                serialized,
                                serialized.len(),
                            )
                            .await
                            .0
                            .map_err(|error| {
                                error!(
                                    "[STORAGE] Write error for {}: {}",
                                    Debug2Format(
                                        &<StorageKey as num::FromPrimitive>::from_u8(K).unwrap()
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

                let result = invalidate_key(database, &[K, StorageKeyType::Data as u8])
                    .await
                    .0
                    .map_err(|error| {
                        error!("[STORAGE] Delete error: {}", Debug2Format(&error));
                    });
                garbage_collect(database).await.0.unwrap();
                response_channel
                    .send(StorageResponse::Delete(result.map(|_code| {})))
                    .await;
            }
        };
    }
}

pub struct StorageClient<
    T: 'static + DeserializeOwned + Serialize + MaxSize,
    const K: u8,
    const N: usize,
> where
    [(); T::POSTCARD_MAX_SIZE]:,
{
    service: &'static StorageService<T, K, N>,
    response_channel: Channel<ThreadModeRawMutex, StorageResponse<T>, N>,
}

impl<T: 'static + DeserializeOwned + Serialize + MaxSize, const K: u8, const N: usize>
    StorageClient<T, K, N>
where
    [(); T::POSTCARD_MAX_SIZE]:,
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

async fn perform_pending_flash_op<'a, F: NorFlash>(
    database: &mut AsyncTicKV<'a, FlashDevice<F>, { F::ERASE_SIZE }>,
) -> Result<(), ErrorCode> {
    let operation = database.tickv.controller.pending.get();
    database.tickv.controller.pending.set(None);
    match operation {
        Some(PendingOperation::Read(page)) => {
            if database
                .tickv
                .controller
                .read(page * F::ERASE_SIZE)
                .await
                .is_err()
            {
                return Err(ErrorCode::ReadFail);
            }
            database.set_read_buffer(database.tickv.controller.op_buf.borrow_mut().as_mut());
        }
        Some(PendingOperation::Write(address, len)) => {
            // Data should already by contained in `op_buf`, so we just need to pass the length of
            // the data and the address to write to.
            if database.tickv.controller.write(address, len).await.is_err() {
                return Err(ErrorCode::WriteFail);
            }
        }
        Some(PendingOperation::Delete(page)) => {
            if database
                .tickv
                .controller
                .erase(page * F::ERASE_SIZE)
                .await
                .is_err()
            {
                return Err(ErrorCode::EraseFail);
            }
        }
        _ => {}
    }
    Ok(())
}

async fn continue_to_completion<'a, F: NorFlash>(
    database: &mut AsyncTicKV<'a, FlashDevice<F>, { F::ERASE_SIZE }>,
) -> (
    Result<SuccessCode, ErrorCode>,
    Option<&'static mut [u8]>,
    usize,
) {
    let ret = loop {
        // Perform the last called AsyncTicKV operation to completion
        if let Err(e) = perform_pending_flash_op(database).await {
            break (Err(e), None, 0);
        };
        let (result, buf, len) = database.continue_operation();
        match result {
            // These errors occur when we want to call an async flash operation.
            // We continue the loop to handle them with `perform_pending_flash_op`
            Err(ErrorCode::ReadNotReady(_))
            | Err(ErrorCode::WriteNotReady(_))
            | Err(ErrorCode::EraseNotReady(_)) => {}
            _ => {
                break (result, buf, len);
            }
        }
    };

    // Take care of any leftover pending flash operations (usually a write) when the TicKV operation is complete
    perform_pending_flash_op(database).await.unwrap();

    ret
}

async fn initialise<'a, F: NorFlash>(
    database: &mut AsyncTicKV<'a, FlashDevice<F>, { F::ERASE_SIZE }>,
) -> Result<SuccessCode, ErrorCode> {
    let mut ret = database.initialise(get_hashed_key(MAIN_KEY));
    if ret.is_err() {
        ret = continue_to_completion(database).await.0;
    }
    ret
}

async fn append_key<'a, F: NorFlash>(
    database: &mut AsyncTicKV<'a, FlashDevice<F>, { F::ERASE_SIZE }>,
    key: &[u8],
    value: &'static mut [u8],
    length: usize,
) -> (
    Result<SuccessCode, ErrorCode>,
    Option<&'static mut [u8]>,
    usize,
) {
    let ret = database.append_key(get_hashed_key(key), value, length);
    match ret {
        Ok(SuccessCode::Queued) => continue_to_completion(database).await,
        _ => unreachable!(),
    }
}

async fn get_key<'a, F: NorFlash>(
    database: &mut AsyncTicKV<'a, FlashDevice<F>, { F::ERASE_SIZE }>,
    key: &[u8],
    buf: &'static mut [u8],
) -> (
    Result<SuccessCode, ErrorCode>,
    Option<&'static mut [u8]>,
    usize,
) {
    let ret = database.get_key(get_hashed_key(key), buf);
    match ret {
        Ok(SuccessCode::Queued) => continue_to_completion(database).await,
        _ => unreachable!(),
    }
}

async fn invalidate_key<'a, F: NorFlash>(
    database: &mut AsyncTicKV<'a, FlashDevice<F>, { F::ERASE_SIZE }>,
    key: &[u8],
) -> (
    Result<SuccessCode, ErrorCode>,
    Option<&'static mut [u8]>,
    usize,
) {
    let ret = database.invalidate_key(get_hashed_key(key));
    match ret {
        Ok(SuccessCode::Queued) => continue_to_completion(database).await,
        _ => unreachable!(),
    }
}

async fn garbage_collect<'a, F: NorFlash>(
    database: &mut AsyncTicKV<'a, FlashDevice<F>, { F::ERASE_SIZE }>,
) -> (
    Result<SuccessCode, ErrorCode>,
    Option<&'static mut [u8]>,
    usize,
) {
    let ret = database.garbage_collect();
    match ret {
        Ok(SuccessCode::Queued) => continue_to_completion(database).await,
        _ => unreachable!(),
    }
}

#[rumcake_macros::task]
pub async fn storage_task<F: NorFlash>(driver: FlashDevice<F>)
where
    [(); F::ERASE_SIZE]:,
{
    let mut read_buf = [0; F::ERASE_SIZE];
    let flash_size = driver.end - driver.start;
    let mut database = tickv::AsyncTicKV::new(driver, &mut read_buf, flash_size);

    // Initialize the database, formatting if needed
    initialise(&mut database).await.unwrap();

    // Create state objects for TicKV services
    #[cfg(feature = "backlight")]
    static mut BACKLIGHT_STATE: StorageServiceState<crate::backlight::animations::BacklightConfig> =
        StorageServiceState::new();
    #[cfg(feature = "underglow")]
    static mut UNDERGLOW_STATE: StorageServiceState<crate::underglow::animations::UnderglowConfig> =
        StorageServiceState::new();

    // Initialize all services
    unsafe {
        #[cfg(feature = "backlight")]
        crate::backlight::BACKLIGHT_CONFIG_STORAGE_SERVICE
            .initialize(&mut database, &mut BACKLIGHT_STATE)
            .await
            .unwrap();
        #[cfg(feature = "underglow")]
        crate::underglow::UNDERGLOW_CONFIG_STORAGE_SERVICE
            .initialize(&mut database, &mut UNDERGLOW_STATE)
            .await
            .unwrap();
    }

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
                unsafe {
                    while let Ok((req, response_channel)) =
                        crate::backlight::BACKLIGHT_CONFIG_STORAGE_SERVICE
                            .requests
                            .try_receive()
                    {
                        crate::backlight::BACKLIGHT_CONFIG_STORAGE_SERVICE
                            .handle_request(
                                &mut database,
                                &mut BACKLIGHT_STATE,
                                req,
                                response_channel,
                            )
                            .await;
                    }
                }
            }
            1 => {
                #[cfg(feature = "underglow")]
                unsafe {
                    while let Ok((req, response_channel)) =
                        crate::underglow::UNDERGLOW_CONFIG_STORAGE_SERVICE
                            .requests
                            .try_receive()
                    {
                        crate::underglow::UNDERGLOW_CONFIG_STORAGE_SERVICE
                            .handle_request(
                                &mut database,
                                &mut UNDERGLOW_STATE,
                                req,
                                response_channel,
                            )
                            .await;
                    }
                }
            }
            _ => {}
        };
    }
}
