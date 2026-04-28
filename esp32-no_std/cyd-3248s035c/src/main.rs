#![no_std]
#![no_main]

extern crate alloc;

use alloc::rc::Rc;
use alloc::string::ToString;
use core::{cell::RefCell, fmt::Write};

use embassy_executor::Spawner;
use embassy_sync::blocking_mutex::{Mutex, raw::CriticalSectionRawMutex};
use embassy_time::{Duration, Timer};
use embedded_hal_bus::spi::ExclusiveDevice;
use esp_backtrace as _;
use esp_hal::{
    analog::adc::{Adc, AdcConfig, AdcPin, Attenuation},
    clock::CpuClock,
    delay::Delay,
    gpio::{AnyPin, Input, InputConfig, Level, Output, OutputConfig, Pull},
    i2c::master::{Config as I2cConfig, I2c},
    peripherals::{ADC1, GPIO0, GPIO2, GPIO4, GPIO15, GPIO16, GPIO17, GPIO25, GPIO32, GPIO33, GPIO34, I2C0},
    spi::{Mode, master::{Config as SpiConfig, Spi}},
    time::Rate,
    timer::timg::TimerGroup,
};
use heapless::String;
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
const GT911_ADDR: u8 = 0x14;
const TOUCH_X_MIN: i32 = 2;
const TOUCH_X_MAX: i32 = 319;
const TOUCH_Y_MIN: i32 = 4;
const TOUCH_Y_MAX: i32 = 471;

static SHARED: Mutex<CriticalSectionRawMutex, RefCell<SharedState>> =
    Mutex::new(RefCell::new(SharedState::new()));

#[derive(Clone, Copy)]
struct SharedState {
    led_color: &'static str,
    light_adc: u16,
    boot_pressed: bool,
    dark_mode: bool,
    tap_count: u32,
    touch_x: i32,
    touch_y: i32,
    touch_active: bool,
    page: i32,
    tick: i32,
}

impl SharedState {
    const fn new() -> Self {
        Self {
            led_color: "RED",
            light_adc: 0,
            boot_pressed: false,
            dark_mode: false,
            tap_count: 0,
            touch_x: 0,
            touch_y: 0,
            touch_active: false,
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

fn transform_touch(raw_x: u16, raw_y: u16) -> (i32, i32) {
    let measured_x = raw_y as i32;
    let measured_y = (raw_x as i32 * SCREEN_H as i32) / 320;
    let rotated_x = (measured_x - TOUCH_X_MIN) * (SCREEN_W as i32 - 1) / (TOUCH_X_MAX - TOUCH_X_MIN);
    let rotated_y = (measured_y - TOUCH_Y_MIN) * (SCREEN_H as i32 - 1) / (TOUCH_Y_MAX - TOUCH_Y_MIN);
    let x = (SCREEN_W as i32 - 1) - (rotated_y * (SCREEN_W as i32 - 1) / (SCREEN_H as i32 - 1));
    let y = (SCREEN_H as i32 - 1) - (rotated_x * (SCREEN_W as i32 - 1) / (SCREEN_W as i32 - 1));
    // Flip 180° for cable-down orientation
    let x = (SCREEN_W as i32 - 1) - x;
    let y = (SCREEN_H as i32 - 1) - y;
    (x.clamp(0, SCREEN_W as i32 - 1), y.clamp(0, SCREEN_H as i32 - 1))
}

fn led_color_to_rgb(name: &str) -> (bool, bool, bool) {
    match name {
        "RED" => (true, false, false),
        "GREEN" => (false, true, false),
        "BLUE" => (false, false, true),
        "YELLOW" => (true, true, false),
        "CYAN" => (false, true, true),
        "MAGENTA" => (true, false, true),
        "WHITE" => (true, true, true),
        _ => (false, false, false),
    }
}

fn set_led(output: &mut Output<'static>, on: bool) {
    if on { output.set_low(); } else { output.set_high(); }
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
async fn touch_task(
    i2c0: I2C0<'static>,
    scl: GPIO32<'static>,
    sda: GPIO33<'static>,
    rst: GPIO25<'static>,
    window: Rc<slint::platform::software_renderer::MinimalSoftwareWindow>,
) -> ! {
    let mut reset = Output::new(rst, Level::High, OutputConfig::default());
    let delay = Delay::new();
    reset.set_low();
    delay.delay_millis(10);
    reset.set_high();
    delay.delay_millis(255);

    let i2c = I2c::new(i2c0, I2cConfig::default().with_frequency(Rate::from_khz(400)))
        .unwrap()
        .with_sda(sda)
        .with_scl(scl);

    let mut i2c = i2c.into_async();

    esp_println::println!("GT911 init");

    let mut probe_buf = [0u8; 4];
    let mut found = false;
    let mut chip_addr: u8 = GT911_ADDR;

    for &addr in &[0x5D, 0x14] {
        let _ = i2c.write_async(addr, &[0x80, 0x40, 0]).await;
        match i2c.write_read_async(addr, &[0x81, 0x40], &mut probe_buf).await {
            Ok(()) if &probe_buf[..3] == b"911" => {
                found = true;
                chip_addr = addr;
                let _ = i2c.write_async(addr, &[0x81, 0x4E, 0x00]).await;
                esp_println::println!("GT911 at 0x{:02X}", addr);
                break;
            }
            _ => {}
        }
    }

    if !found {
        esp_println::println!("No GT911 found");
        loop { Timer::after(Duration::from_secs(5)).await; }
    }

    let mut last_touch_pos: Option<slint::LogicalPosition> = None;

    loop {
        let mut status_buf = [0u8; 1];
        let mut touch_buf = [0u8; 8];

        match i2c.write_read_async(chip_addr, &[0x81, 0x4E], &mut status_buf).await {
            Ok(()) => {
                let status = status_buf[0];
                let buffer_ready = (status & 0x80) != 0;
                let num_points = status & 0x0F;

                if buffer_ready && num_points > 0 {
                    match i2c.write_read_async(chip_addr, &[0x81, 0x4F], &mut touch_buf).await {
                        Ok(()) => {
                            let raw_x = u16::from_le_bytes([touch_buf[1], touch_buf[2]]);
                            let raw_y = u16::from_le_bytes([touch_buf[3], touch_buf[4]]);
                            let (x, y) = transform_touch(raw_x, raw_y);

                            let pos = slint::PhysicalPosition::new(x, y).to_logical(window.scale_factor());

                            match last_touch_pos {
                                None => {
                                    window.dispatch_event(
                                        slint::platform::WindowEvent::PointerPressed {
                                            position: pos,
                                            button: slint::platform::PointerEventButton::Left,
                                        },
                                    );
                                    with_shared(|s| {
                                        s.tap_count = s.tap_count.wrapping_add(1);
                                        s.touch_active = true;
                                        s.touch_x = x;
                                        s.touch_y = y;
                                    });
                                }
                                Some(prev) if prev != pos => {
                                    window.dispatch_event(
                                        slint::platform::WindowEvent::PointerMoved { position: pos },
                                    );
                                    with_shared(|s| {
                                        s.touch_x = x;
                                        s.touch_y = y;
                                    });
                                }
                                _ => {}
                            }
                            last_touch_pos = Some(pos);
                        }
                        Err(_) => {}
                    }
                } else {
                    if let Some(pos) = last_touch_pos.take() {
                        window.dispatch_event(
                            slint::platform::WindowEvent::PointerReleased {
                                position: pos,
                                button: slint::platform::PointerEventButton::Left,
                            },
                        );
                        window.dispatch_event(slint::platform::WindowEvent::PointerExited);
                        with_shared(|s| s.touch_active = false);
                    }
                }

                let _ = i2c.write_async(chip_addr, &[0x81, 0x4E, 0x00]).await;
            }
            Err(_) => {}
        }

        Timer::after(Duration::from_millis(16)).await;
    }
}

const NUM_PAGES: i32 = 7;
const LONG_PRESS_MS: u64 = 500;

#[embassy_executor::task]
async fn sensor_task(adc1: ADC1<'static>, photo: GPIO34<'static>, boot: GPIO0<'static>) -> ! {
    let mut adc_config = AdcConfig::new();
    let mut photo_pin: AdcPin<GPIO34<'static>, ADC1<'static>> =
        adc_config.enable_pin(photo, Attenuation::_11dB);
    let mut adc = Adc::new(adc1, adc_config);
    let boot = Input::new(boot, InputConfig::default().with_pull(Pull::Up));
    let mut press_start: Option<embassy_time::Instant> = None;
    let mut adc_divider: u8 = 0;

    loop {
        // Read ADC every ~400ms (8 × 50ms)
        adc_divider = adc_divider.wrapping_add(1);
        if adc_divider % 8 == 0 {
            let adc_value = loop {
                match adc.read_oneshot(&mut photo_pin) {
                    Ok(v) => break v,
                    Err(_) => {}
                }
            };
            with_shared(|s| s.light_adc = adc_value);
        }

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
async fn rgb_task(red_pin: GPIO4<'static>, green_pin: GPIO16<'static>, blue_pin: GPIO17<'static>) -> ! {
    let mut red = Output::new(red_pin, Level::High, OutputConfig::default());
    let mut green = Output::new(green_pin, Level::High, OutputConfig::default());
    let mut blue = Output::new(blue_pin, Level::High, OutputConfig::default());
    let mut last = "";

    loop {
        let color = snapshot().led_color;
        if color != last {
            let (r, g, b) = led_color_to_rgb(color);
            set_led(&mut red, r);
            set_led(&mut green, g);
            set_led(&mut blue, b);
            last = color;
        }
        Timer::after(Duration::from_millis(40)).await;
    }
}

#[embassy_executor::task]
async fn ui_sync_task(main_window: MainWindow) -> ! {
    let mut last_tap = 0u32;
    let mut last_dark = false;

    loop {
        let state = snapshot();
        let globals = main_window.global::<HardwareGlobals>();

        globals.set_light_value(state.light_adc as i32);
        globals.set_boot_pressed(state.boot_pressed);
        globals.set_tap_count(state.tap_count as i32);

        if state.dark_mode != last_dark {
            globals.set_dark_mode(state.dark_mode);
            last_dark = state.dark_mode;
        }

        if state.touch_active {
            let mut pos: String<32> = String::new();
            let _ = write!(pos, "x={} y={}", state.touch_x, state.touch_y);
            globals.set_touch_pos(slint::SharedString::from(pos.as_str()));
        } else {
            globals.set_touch_pos(slint::SharedString::from("idle"));
        }

        globals.set_page(state.page);
        globals.set_tick(state.tick);

        if state.tap_count != last_tap {
            globals.set_led_color(slint::SharedString::from(state.led_color));
            last_tap = state.tap_count;
        }

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
    globals.on_led_color_changed(|color| {
        let name = color.to_string();
        let static_name: &'static str = match name.as_str() {
            "RED" => "RED",
            "GREEN" => "GREEN",
            "BLUE" => "BLUE",
            "YELLOW" => "YELLOW",
            "CYAN" => "CYAN",
            "MAGENTA" => "MAGENTA",
            "WHITE" => "WHITE",
            _ => "OFF",
        };
        with_shared(|s| s.led_color = static_name);
        esp_println::println!("LED color: {}", static_name);
    });

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

    spawner.spawn(touch_task(
        peripherals.I2C0,
        peripherals.GPIO32,
        peripherals.GPIO33,
        peripherals.GPIO25,
        slint_window.clone(),
    )).unwrap();

    spawner.spawn(sensor_task(peripherals.ADC1, peripherals.GPIO34, peripherals.GPIO0)).unwrap();
    spawner.spawn(rgb_task(peripherals.GPIO4, peripherals.GPIO16, peripherals.GPIO17)).unwrap();
    spawner.spawn(ui_sync_task(main_window)).unwrap();

    loop {
        Timer::after(Duration::from_secs(2)).await;
    }
}
