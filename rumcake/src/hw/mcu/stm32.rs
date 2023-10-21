use core::fmt::Debug;
use embassy_stm32::bind_interrupts;
use embassy_stm32::dma::NoDma;
use embassy_stm32::flash::{Bank1Region, Blocking, Flash};
use embassy_stm32::i2c::I2c;
use embassy_stm32::peripherals::{FLASH, PA11, PA12, USB};
use embassy_stm32::time::Hertz;
use embassy_stm32::usb::Driver;
use embassy_sync::blocking_mutex::raw::ThreadModeRawMutex;
use embassy_sync::mutex::Mutex;
use embedded_hal::blocking::i2c::Write;
use embedded_hal_async::i2c::I2c as AsyncI2c;
use embedded_storage::nor_flash::NorFlash;
use static_cell::StaticCell;

#[cfg(feature = "stm32f072cb")]
pub const SYSCLK: u32 = 48_000_000;

#[cfg(feature = "stm32f303cb")]
pub const SYSCLK: u32 = 72_000_000;

pub fn initialize_rcc() {
    let mut conf = embassy_stm32::Config::default();
    let mut rcc_conf = embassy_stm32::rcc::Config::default();

    #[cfg(feature = "stm32f072cb")]
    {
        rcc_conf.sys_ck = Some(embassy_stm32::time::Hertz(SYSCLK));
    }

    #[cfg(feature = "stm32f303cb")]
    {
        rcc_conf.sysclk = Some(embassy_stm32::time::Hertz(SYSCLK));
        rcc_conf.hse = Some(embassy_stm32::time::Hertz(8_000_000));
        rcc_conf.pclk1 = Some(embassy_stm32::time::Hertz(24_000_000));
        rcc_conf.pclk2 = Some(embassy_stm32::time::Hertz(24_000_000));
    }

    conf.rcc = rcc_conf;

    embassy_stm32::init(conf);
}

#[macro_export]
macro_rules! input_pin {
    ($p:ident) => {
        unsafe {
            $crate::embassy_stm32::gpio::Input::new(
                $crate::embassy_stm32::gpio::Pin::degrade(
                    $crate::embassy_stm32::peripherals::$p::steal(),
                ),
                $crate::embassy_stm32::gpio::Pull::Up,
            )
        }
    };
}

#[macro_export]
macro_rules! output_pin {
    ($p:ident) => {
        unsafe {
            $crate::embassy_stm32::gpio::Output::new(
                $crate::embassy_stm32::gpio::Pin::degrade(
                    $crate::embassy_stm32::peripherals::$p::steal(),
                ),
                $crate::embassy_stm32::gpio::Level::High,
                $crate::embassy_stm32::gpio::Speed::Low,
            )
        }
    };
}

#[cfg(feature = "usb")]
pub fn setup_usb_driver<K: crate::usb::USBKeyboard>(
) -> embassy_usb::Builder<'static, Driver<'static, USB>> {
    unsafe {
        #[cfg(feature = "stm32f072cb")]
        bind_interrupts!(
            struct Irqs {
                USB => embassy_stm32::usb::InterruptHandler<embassy_stm32::peripherals::USB>;
            }
        );

        #[cfg(feature = "stm32f303cb")]
        bind_interrupts!(
            struct Irqs {
                USB_LP_CAN_RX0 => embassy_stm32::usb::InterruptHandler<embassy_stm32::peripherals::USB>;
            }
        );

        let mut config = embassy_usb::Config::new(K::USB_VID, K::USB_PID);
        config.manufacturer.replace(K::MANUFACTURER);
        config.product.replace(K::PRODUCT);
        config.serial_number.replace(K::SERIAL_NUMBER);
        config.max_power = 500;

        let usb_driver = Driver::new(USB::steal(), Irqs, PA12::steal(), PA11::steal());

        static DEVICE_DESCRIPTOR: static_cell::StaticCell<[u8; 256]> =
            static_cell::StaticCell::new();
        let device_descriptor = DEVICE_DESCRIPTOR.init([0; 256]);
        static CONFIG_DESCRIPTOR: static_cell::StaticCell<[u8; 256]> =
            static_cell::StaticCell::new();
        let config_descriptor = CONFIG_DESCRIPTOR.init([0; 256]);
        static BOS_DESCRIPTOR: static_cell::StaticCell<[u8; 256]> = static_cell::StaticCell::new();
        let bos_descriptor = BOS_DESCRIPTOR.init([0; 256]);
        static CONTROL_BUF: static_cell::StaticCell<[u8; 128]> = static_cell::StaticCell::new();
        let control_buf = CONTROL_BUF.init([0; 128]);

        embassy_usb::Builder::new(
            usb_driver,
            config,
            device_descriptor,
            config_descriptor,
            bos_descriptor,
            control_buf,
        )
    }
}

pub fn setup_flash() -> &'static mut Mutex<ThreadModeRawMutex, impl NorFlash> {
    unsafe {
        static FLASH_PERIPHERAL: StaticCell<Mutex<ThreadModeRawMutex, Bank1Region<Blocking>>> =
            StaticCell::new();

        FLASH_PERIPHERAL.init(Mutex::new(
            Flash::new_blocking(FLASH::steal())
                .into_blocking_regions()
                .bank1_region,
        ))
    }
}

pub fn setup_internal_flash() -> impl NorFlash {
    unsafe { Flash::new_blocking(FLASH::steal()) }
}

#[macro_export]
macro_rules! setup_i2c {
    ($interrupt:ident, $i2c:ident, $scl:ident, $sda:ident, $rxdma:ident, $txdma:ident) => {
        fn setup_i2c() -> impl $crate::embedded_hal_async::i2c::I2c<Error = impl core::fmt::Debug> {
            unsafe {
                $crate::embassy_stm32::bind_interrupts! {
                    struct Irqs {
                        $interrupt => $crate::embassy_stm32::i2c::InterruptHandler<$crate::embassy_stm32::peripherals::$i2c>;
                    }
                };
                let i2c = $crate::embassy_stm32::peripherals::$i2c::steal();
                let scl = $crate::embassy_stm32::peripherals::$scl::steal();
                let sda = $crate::embassy_stm32::peripherals::$sda::steal();
                let rx_dma = $crate::embassy_stm32::peripherals::$rxdma::steal();
                let tx_dma = $crate::embassy_stm32::peripherals::$txdma::steal();
                let time = $crate::embassy_stm32::time::Hertz(100_000);
                $crate::embassy_stm32::i2c::I2c::new(i2c, scl, sda, Irqs, tx_dma, rx_dma, time, Default::default())
            }
        }
    };
}

#[macro_export]
macro_rules! setup_uart_reader {
    ($interrupt:ident, $uart:ident, $rx:ident, $rxdma:ident) => {
        fn setup_uart_reader() -> impl $crate::embedded_io_async::Read<Error = impl core::fmt::Debug> {
            unsafe {
                $crate::embassy_stm32::bind_interrupts! {
                    struct Irqs {
                        $interrupt => $crate::embassy_stm32::usart::InterruptHandler<$crate::embassy_stm32::peripherals::$uart>;
                    }
                };
                let uart = $crate::embassy_stm32::peripherals::$uart::steal();
                let rx = $crate::embassy_stm32::peripherals::$rx::steal();
                let rx_dma = $crate::embassy_stm32::peripherals::$rxdma::steal();
                $crate::embassy_stm32::usart::UartRx::new(uart, Irqs, rx, rx_dma, Default::default()).into_ring_buffered(&mut [0; 32]);
            }
        }
    };
}

#[macro_export]
macro_rules! setup_uart_writer {
    ($interrupt:ident, $uart:ident, $tx:ident, $txdma:ident) => {
        fn setup_uart_writer(
        ) -> impl $crate::embedded_io_async::Write<Error = impl core::fmt::Debug> {
            unsafe {
                let uart = $crate::embassy_stm32::peripherals::$uart::steal();
                let tx = $crate::embassy_stm32::peripherals::$tx::steal();
                let tx_dma = $crate::embassy_stm32::peripherals::$txdma::steal();
                $crate::embassy_stm32::usart::UartTx::new(uart, tx, tx_dma, Default::default())
            }
        }
    };
}
