#![no_std]
#![no_main]

extern crate alloc;

use alloc::rc::Rc;
use core::cell::RefCell;

use embassy_executor::Spawner;
use embassy_sync::blocking_mutex::{Mutex, raw::CriticalSectionRawMutex};
use embassy_time::{Duration, Timer};
use embedded_hal_bus::spi::ExclusiveDevice;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    delay::Delay,
    gpio::{AnyPin, Input, InputConfig, Level, Output, OutputConfig, Pull},
    peripherals::{GPIO0, GPIO2, GPIO15},
    spi::{Mode, master::{Config as SpiConfig, Spi}},
    time::Rate,
    timer::timg::TimerGroup,
};
use mipidsi::{
    Builder,
    interface::SpiInterface,
    models::ST7796,
    options::{ColorInversion, ColorOrder, Orientation},
};
use static_cell::StaticCell;

slint::include_modules!();

esp_bootloader_esp_idf::esp_app_desc!();

const SCREEN_W: u32 = 320;
const SCREEN_H: u32 = 480;
const NUM_PAGES: i32 = 3;
const LONG_PRESS_MS: u64 = 500;

static SHARED: Mutex<CriticalSectionRawMutex, RefCell<SharedState>> =
    Mutex::new(RefCell::new(SharedState::new()));

#[derive(Clone, Copy)]
struct SharedState {
    boot_pressed: bool,
    dark_mode: bool,
    page: i32,
    tick: i32,
}

impl SharedState {
    const fn new() -> Self {
        Self {
            boot_pressed: false,
            dark_mode: false,
            page: 0,
            tick: 0,
        }
    }
}

fn with_shared<R>(f: impl FnOnce(&mut SharedState) -> R) -> R {
    SHARED.lock(|s| f(&mut s.borrow_mut()))
}

fn snapshot() -> SharedState {
    SHARED.lock(|s| *s.borrow())
}

static SLINT_WINDOW: StaticCell<Rc<slint::platform::software_renderer::MinimalSoftwareWindow>> = StaticCell::new();

struct EspBackend {
    window: Rc<slint::platform::software_renderer::MinimalSoftwareWindow>,
}

impl slint::platform::Platform for EspBackend {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        self.window.set_size(slint::PhysicalSize::new(SCREEN_W, SCREEN_H));
        Ok(self.window.clone())
    }

    fn duration_since_start(&self) -> core::time::Duration {
        let ms = embassy_time::Instant::now().as_millis();
        core::time::Duration::from_millis(ms as u64)
    }
}

struct LineRenderer<'a, D> {
    display: D,
    buffer: &'a mut [slint::platform::software_renderer::Rgb565Pixel],
}

impl<DI> slint::platform::software_renderer::LineBufferProvider
    for &mut LineRenderer<'_, &mut mipidsi::Display<DI, ST7796, mipidsi::NoResetPin>>
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
        let line_buf = &mut self.buffer[range.clone()];
        render_fn(line_buf);

        self.display
            .set_pixels(
                range.start as u16,
                line as u16,
                range.end as u16,
                line as u16,
                line_buf
                    .iter()
                    .map(|p| embedded_graphics::pixelcolor::raw::RawU16::new(p.0).into()),
            )
            .ok();
    }
}

#[embassy_executor::task]
async fn render_loop(
    window: Rc<slint::platform::software_renderer::MinimalSoftwareWindow>,
    spi: Spi<'static, esp_hal::Blocking>,
    cs_pin: GPIO15<'static>,
    dc_pin: GPIO2<'static>,
    backlight_pin: AnyPin<'static>,
) -> ! {
    let cs = Output::new(cs_pin, Level::High, OutputConfig::default());
    let spi_device = ExclusiveDevice::new_no_delay(spi, cs).unwrap();
    let dc = Output::new(dc_pin, Level::Low, OutputConfig::default());
    static DISP_BUF: StaticCell<[u8; 512]> = StaticCell::new();
    let buf = DISP_BUF.init([0; 512]);
    let di = SpiInterface::new(spi_device, dc, buf);
    let mut delay = Delay::new();

    let mut display = Builder::new(ST7796, di)
        .display_size(320, 480)
        .color_order(ColorOrder::Bgr)
        .invert_colors(ColorInversion::Inverted)
        .orientation(Orientation::default().flip_horizontal())
        .init(&mut delay)
        .unwrap();

    let mut backlight = Output::new(backlight_pin, Level::Low, OutputConfig::default());
    backlight.set_high();

    static LINE_BUF: StaticCell<[slint::platform::software_renderer::Rgb565Pixel; 320]> = StaticCell::new();
    let line_buffer = LINE_BUF.init([slint::platform::software_renderer::Rgb565Pixel(0); 320]);

    let mut renderer = LineRenderer {
        display: &mut display,
        buffer: line_buffer,
    };

    esp_println::println!("Slint render loop started");

    let mut frame_count: u32 = 0;

    loop {
        slint::platform::update_timers_and_animations();

        window.draw_if_needed(|renderer_instance| {
            renderer_instance.render_by_line(&mut renderer);
        });

        Timer::after(Duration::from_millis(33)).await;

        frame_count = frame_count.wrapping_add(1);
        if frame_count % 10 == 0 {
            with_shared(|s| s.tick = s.tick.wrapping_add(1));
        }
    }
}

#[embassy_executor::task]
async fn button_task(boot: GPIO0<'static>) -> ! {
    let boot = Input::new(boot, InputConfig::default().with_pull(Pull::Up));
    let mut press_start: Option<embassy_time::Instant> = None;

    loop {
        let boot_pressed = boot.is_low();

        if boot_pressed && press_start.is_none() {
            press_start = Some(embassy_time::Instant::now());
        } else if !boot_pressed {
            if let Some(start) = press_start.take() {
                let held_ms = (embassy_time::Instant::now() - start).as_millis();
                if held_ms >= LONG_PRESS_MS {
                    with_shared(|s| s.dark_mode = !s.dark_mode);
                    esp_println::println!("BOOT long press: dark mode toggle");
                } else {
                    with_shared(|s| s.page = (s.page + 1) % NUM_PAGES);
                    esp_println::println!("BOOT short press: next page");
                }
            }
        }

        with_shared(|s| s.boot_pressed = boot_pressed);

        Timer::after(Duration::from_millis(50)).await;
    }
}

#[embassy_executor::task]
async fn ui_sync_task(main_window: MainWindow) -> ! {
    let mut last_dark = false;

    loop {
        let state = snapshot();
        let globals = main_window.global::<HardwareGlobals>();

        globals.set_boot_pressed(state.boot_pressed);

        if state.dark_mode != last_dark {
            globals.set_dark_mode(state.dark_mode);
            last_dark = state.dark_mode;
        }

        globals.set_page(state.page);
        globals.set_tick(state.tick);

        Timer::after(Duration::from_millis(100)).await;
    }
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();
    esp_println::println!("CYD-3248S035 Slint demo starting!");

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    unsafe {
        const HEAP_SIZE: usize = 100 * 1024;
        static mut HEAP_MEM: core::mem::MaybeUninit<[u8; HEAP_SIZE]> = core::mem::MaybeUninit::uninit();
        let ptr = core::ptr::addr_of_mut!(HEAP_MEM) as *mut u8;
        esp_alloc::HEAP.add_region(esp_alloc::HeapRegion::new(
            ptr,
            HEAP_SIZE,
            esp_alloc::MemoryCapability::Internal.into(),
        ));
    }

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0);

    let slint_window = SLINT_WINDOW.init(
        slint::platform::software_renderer::MinimalSoftwareWindow::new(
            slint::platform::software_renderer::RepaintBufferType::ReusedBuffer,
        )
    );

    let backend = EspBackend {
        window: slint_window.clone(),
    };
    slint::platform::set_platform(alloc::boxed::Box::new(backend))
        .expect("Failed to set Slint platform");

    let main_window = MainWindow::new().unwrap();

    let globals = main_window.global::<HardwareGlobals>();
    globals.on_page_changed(|page| {
        with_shared(|s| s.page = page);
        esp_println::println!("Page: {}", page);
    });

    let spi = Spi::new(
        peripherals.SPI2,
        SpiConfig::default()
            .with_frequency(Rate::from_mhz(10))
            .with_mode(Mode::_0),
    )
    .unwrap()
    .with_sck(peripherals.GPIO14)
    .with_mosi(peripherals.GPIO13)
    .with_miso(peripherals.GPIO12);

    spawner.spawn(render_loop(
        slint_window.clone(),
        spi,
        peripherals.GPIO15,
        peripherals.GPIO2,
        unsafe { AnyPin::steal(27) },
    )).unwrap();

    spawner.spawn(button_task(peripherals.GPIO0)).unwrap();
    spawner.spawn(ui_sync_task(main_window)).unwrap();

    loop {
        Timer::after(Duration::from_secs(2)).await;
    }
}
