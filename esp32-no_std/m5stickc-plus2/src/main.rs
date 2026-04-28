#![no_std]
#![no_main]

extern crate alloc;

use alloc::{boxed::Box, rc::Rc};
use core::cell::RefCell;

use embassy_executor::Spawner;
use embassy_sync::blocking_mutex::{Mutex, raw::CriticalSectionRawMutex};
use embassy_time::{Duration, Timer};
use embedded_hal_bus::spi::ExclusiveDevice;
use esp_backtrace as _;
use esp_hal::{
    Blocking,
    clock::CpuClock,
    delay::Delay,
    gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull},
    peripherals::{GPIO5, GPIO12, GPIO14, GPIO27, GPIO35, GPIO37, GPIO39},
    spi::{
        Mode,
        master::{Config as SpiConfig, Spi},
    },
    time::Rate,
    timer::timg::TimerGroup,
};
use mipidsi::{
    models::ST7789,
    options::{ColorInversion, ColorOrder},
};
use static_cell::StaticCell;

slint::include_modules!();

esp_bootloader_esp_idf::esp_app_desc!();

const LCD_H_RES: u16 = 135;
const LCD_V_RES: u16 = 240;
const PAGE_COUNT: i32 = 3;
const DISPLAY_OFFSET_X: u16 = 52;
const DISPLAY_OFFSET_Y: u16 = 40;

static SHARED: Mutex<CriticalSectionRawMutex, RefCell<SharedState>> =
    Mutex::new(RefCell::new(SharedState::new()));
static SLINT_WINDOW: StaticCell<Rc<slint::platform::software_renderer::MinimalSoftwareWindow>> =
    StaticCell::new();

#[derive(Clone, Copy)]
struct SharedState {
    page: i32,
    dark_mode: bool,
    tick: i32,
    button_next: bool,
    button_prev: bool,
    button_dark: bool,
}

impl SharedState {
    const fn new() -> Self {
        Self {
            page: 0,
            dark_mode: false,
            tick: 0,
            button_next: false,
            button_prev: false,
            button_dark: false,
        }
    }
}

fn with_shared<R>(f: impl FnOnce(&mut SharedState) -> R) -> R {
    SHARED.lock(|state| f(&mut state.borrow_mut()))
}

struct EspBackend {
    window: Rc<slint::platform::software_renderer::MinimalSoftwareWindow>,
}

impl slint::platform::Platform for EspBackend {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        self.window
            .set_size(slint::PhysicalSize::new(LCD_H_RES as u32, LCD_V_RES as u32));
        Ok(self.window.clone())
    }

    fn duration_since_start(&self) -> core::time::Duration {
        core::time::Duration::from_millis(embassy_time::Instant::now().as_millis() as u64)
    }
}

struct LineRenderer<'a, Display> {
    display: &'a mut Display,
    buffer: &'a mut [slint::platform::software_renderer::Rgb565Pixel],
}

impl<'a, Display> LineRenderer<'a, Display> {
    fn new(
        display: &'a mut Display,
        buffer: &'a mut [slint::platform::software_renderer::Rgb565Pixel],
    ) -> Self {
        Self { display, buffer }
    }
}

impl<DI> slint::platform::software_renderer::LineBufferProvider
    for &mut LineRenderer<'_, mipidsi::Display<DI, ST7789, Output<'static>>>
where
    DI: mipidsi::interface::Interface<Word = u8>,
{
    type TargetPixel = slint::platform::software_renderer::Rgb565Pixel;

    fn process_line(
        &mut self,
        line: usize,
        range: core::ops::Range<usize>,
        render_fn: impl FnOnce(&mut [slint::platform::software_renderer::Rgb565Pixel]),
    ) {
        let buffer = &mut self.buffer[range.clone()];
        render_fn(buffer);

        self.display
            .set_pixels(
                range.start as u16,
                line as u16,
                (range.end - 1) as u16,
                line as u16,
                buffer
                    .iter()
                    .map(|pixel| embedded_graphics::pixelcolor::raw::RawU16::new(pixel.0).into()),
            )
            .unwrap();
    }
}

#[embassy_executor::task]
async fn render_loop(
    window: Rc<slint::platform::software_renderer::MinimalSoftwareWindow>,
    spi: Spi<'static, Blocking>,
    cs_pin: GPIO5<'static>,
    dc_pin: GPIO14<'static>,
    rst_pin: GPIO12<'static>,
    backlight_pin: GPIO27<'static>,
) -> ! {
    let cs = Output::new(cs_pin, Level::High, OutputConfig::default());
    let dc = Output::new(dc_pin, Level::Low, OutputConfig::default());
    let rst = Output::new(rst_pin, Level::High, OutputConfig::default());
    let mut backlight = Output::new(backlight_pin, Level::Low, OutputConfig::default());
    let spi_device = ExclusiveDevice::new_no_delay(spi, cs).unwrap();

    static DISPLAY_BUFFER: StaticCell<[u8; 512]> = StaticCell::new();
    let buffer = DISPLAY_BUFFER.init([0; 512]);
    let di = mipidsi::interface::SpiInterface::new(spi_device, dc, buffer);
    let mut delay = Delay::new();

    let mut display = mipidsi::Builder::new(ST7789, di)
        .reset_pin(rst)
        .display_size(LCD_H_RES, LCD_V_RES)
        .display_offset(DISPLAY_OFFSET_X, DISPLAY_OFFSET_Y)
        .color_order(ColorOrder::Bgr)
        .invert_colors(ColorInversion::Inverted)
        .init(&mut delay)
        .unwrap();

    backlight.set_high();

    static LINE_BUFFER: StaticCell<[slint::platform::software_renderer::Rgb565Pixel; LCD_H_RES as usize]> = StaticCell::new();
    let line_buffer =
        LINE_BUFFER.init([slint::platform::software_renderer::Rgb565Pixel(0); LCD_H_RES as usize]);
    let mut renderer = LineRenderer::new(&mut display, line_buffer);

    loop {
        slint::platform::update_timers_and_animations();
        window.draw_if_needed(|software_renderer| {
            software_renderer.render_by_line(&mut renderer);
        });
        Timer::after(Duration::from_millis(16)).await;
    }
}

#[embassy_executor::task]
async fn button_task(
    next_pin: GPIO37<'static>,
    prev_pin: GPIO39<'static>,
    dark_pin: GPIO35<'static>,
) -> ! {
    let next = Input::new(next_pin, InputConfig::default().with_pull(Pull::Up));
    let prev = Input::new(prev_pin, InputConfig::default().with_pull(Pull::Up));
    let dark = Input::new(dark_pin, InputConfig::default().with_pull(Pull::Up));

    let mut last_next = false;
    let mut last_prev = false;
    let mut last_dark = false;

    loop {
        let next_pressed = next.is_low();
        let prev_pressed = prev.is_low();
        let dark_pressed = dark.is_low();

        with_shared(|state| {
            state.button_next = next_pressed;
            state.button_prev = prev_pressed;
            state.button_dark = dark_pressed;

            if next_pressed && !last_next {
                state.page = (state.page + 1) % PAGE_COUNT;
            }

            if prev_pressed && !last_prev {
                state.page = (state.page + PAGE_COUNT - 1) % PAGE_COUNT;
            }

            if dark_pressed && !last_dark {
                state.dark_mode = !state.dark_mode;
            }
        });

        last_next = next_pressed;
        last_prev = prev_pressed;
        last_dark = dark_pressed;

        Timer::after(Duration::from_millis(50)).await;
    }
}

#[embassy_executor::task]
async fn ui_sync_task(main_window: MainWindow) -> ! {
    loop {
        let state = with_shared(|shared| {
            shared.tick = shared.tick.wrapping_add(1);
            *shared
        });

        let globals = main_window.global::<HardwareGlobals>();
        globals.set_page(state.page);
        globals.set_dark_mode(state.dark_mode);
        globals.set_tick(state.tick);

        Timer::after(Duration::from_millis(100)).await;
    }
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();
    esp_println::println!("M5StickC Plus2 Slint demo starting!");

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    let power_hold = esp_hal::gpio::Output::new(
        peripherals.GPIO4,
        esp_hal::gpio::Level::High,
        esp_hal::gpio::OutputConfig::default(),
    );
    core::mem::forget(power_hold);

    esp_alloc::heap_allocator!(size: 72 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0);

    let slint_window = SLINT_WINDOW.init(
        slint::platform::software_renderer::MinimalSoftwareWindow::new(
            slint::platform::software_renderer::RepaintBufferType::ReusedBuffer,
        ),
    );

    slint::platform::set_platform(Box::new(EspBackend {
        window: slint_window.clone(),
    }))
    .expect("Failed to set Slint platform");

    let main_window = MainWindow::new().unwrap();
    main_window.show().unwrap();

    let spi = Spi::new(
        peripherals.SPI2,
        SpiConfig::default()
            .with_frequency(Rate::from_mhz(40))
            .with_mode(Mode::_0),
    )
    .unwrap()
    .with_sck(peripherals.GPIO13)
    .with_mosi(peripherals.GPIO15);

    spawner
        .spawn(render_loop(
            slint_window.clone(),
            spi,
            peripherals.GPIO5,
            peripherals.GPIO14,
            peripherals.GPIO12,
            peripherals.GPIO27,
        ))
        .unwrap();
    spawner
        .spawn(button_task(
            peripherals.GPIO37,
            peripherals.GPIO39,
            peripherals.GPIO35,
        ))
        .unwrap();
    spawner.spawn(ui_sync_task(main_window)).unwrap();

    loop {
        Timer::after(Duration::from_secs(1)).await;
    }
}
