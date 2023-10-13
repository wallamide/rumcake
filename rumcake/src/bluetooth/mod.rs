//! Bluetooth host communication.
//!
//! To use Bluetooth host communication, keyboards must implement [`rumcake::hw::mcu::BluetoothDevice`],
//! and [`BluetoothKeyboard`].

#[cfg(any(all(feature = "nrf", feature = "bluetooth"), doc))]
pub mod nrf_ble;

use embassy_sync::blocking_mutex::raw::ThreadModeRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;

use crate::keyboard::Keyboard;

/// A trait that keyboards must implement to communicate with host devices over Bluetooth (LE).
pub trait BluetoothKeyboard: Keyboard {
    /// Vendor ID for the keyboard.
    const BLE_VID: u16;

    /// Product ID for the keyboard.
    const BLE_PID: u16;

    /// Product version for the keyboard.
    const BLE_PRODUCT_VERSION: &'static str = Self::HARDWARE_REVISION;
}

#[derive(Debug, Clone, Copy)]
/// An enumeration of possible commands that will be processed by the bluetooth task.
pub enum BluetoothCommand {
    #[cfg(feature = "usb")]
    /// Switch between USB and Bluetooth operation.
    ///
    /// This will **NOT** disconnect your keyboard from your host device. It
    /// will simply determine which device the HID reports get sent to.
    ToggleUSB,
}

/// Channel for sending [`BluetoothCommand`]s.
///
/// Channel messages should be consumed by the bluetooth task ([`nrf_ble::nrf_ble_task`] for
/// nRF5x-based keyboards), so user-level code should **not** attempt to receive messages from the
/// channel, otherwise commands may not be processed appropriately. You should only send to this
/// channel.
pub static BLUETOOTH_COMMAND_CHANNEL: Channel<ThreadModeRawMutex, BluetoothCommand, 2> =
    Channel::new();

pub static USB_STATE_LISTENER: Signal<ThreadModeRawMutex, ()> = Signal::new();
pub static BATTERY_LEVEL_LISTENER: Signal<ThreadModeRawMutex, ()> = Signal::new();