//! LilyGo T-Display S3 — Minimal 3-page Slint hardware demo.
//!
//! Hardware: ESP32-S3, ST7789 170×320 LCD via I8080 parallel interface.
//! Dark mode toggled by BOOT button (GPIO0), LED (GPIO2) as visual feedback.
//!
//! Pin references: https://github.com/Xinyuan-LilyGO/T-Display-S3
//!                 examples/tft/pin_config.h

#![no_std]
#![no_main]

esp_bootloader_esp_idf::esp_app_desc!(
    "0.1.0",
    "lilygo-tdisplay-s3",
    "12:00:00",
    "2025-01-01",
    "4.4",
    8 * 1024,
    0,
    u16::MAX
);

extern crate alloc;

use alloc::boxed::Box;
use alloc::rc::Rc;
use core::panic::PanicInfo;
use log::info;

use embassy_time::{Duration, Ticker};
use esp_hal::timer::timg::TimerGroup;

use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::delay::Delay;
use esp_hal::dma::DmaTxBuf;
use esp_hal::dma_tx_buffer;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::lcd_cam::{
    LcdCam,
    lcd::i8080::{Config as I8080Config, I8080, TxEightBits},
};
use esp_hal::time::Rate;
use esp_hal::Blocking;
use esp_println::logger::init_logger_from_env;

use slint::platform::software_renderer::Rgb565Pixel;
use slint::PhysicalSize;

slint::include_modules!();

// ST7789 in landscape rotation (MADCTL=0x60, MY|MV).
// Physical panel 240×320 → 320 wide × 170 tall.
// From TFT_eSPI ST7789_Rotation.h rotation 1: colstart=0, rowstart=35
const LCD_H_RES: u16 = 320;
const LCD_V_RES: u16 = 170;
const ROW_OFFSET: u16 = 35;
const PIXEL_COUNT: usize = LCD_H_RES as usize * LCD_V_RES as usize;

static mut PIXEL_BUF: [Rgb565Pixel; PIXEL_COUNT] = [Rgb565Pixel(0); PIXEL_COUNT];

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    esp_println::println!("*** PANIC: {}", info);
    loop {}
}

// I8080 bus wrapper — consumed by send(), returned by .wait()
struct Bus<'d> {
    resources: Option<(I8080<'d, Blocking>, DmaTxBuf)>,
}

impl<'d> Bus<'d> {
    fn use_resources<T>(
        &mut self,
        f: impl FnOnce(I8080<'d, Blocking>, DmaTxBuf) -> (T, I8080<'d, Blocking>, DmaTxBuf),
    ) -> T {
        let (i8080, buf) = self.resources.take().unwrap();
        let (r, i8080, buf) = f(i8080, buf);
        self.resources = Some((i8080, buf));
        r
    }

    fn send(&mut self, cmd: u8, data: &[u8]) {
        self.use_resources(|i8080, mut buf| {
            buf.as_mut_slice()[..data.len()].copy_from_slice(data);
            buf.set_length(data.len());
            match i8080.send(cmd, 0u8, buf) {
                Ok(transfer) => transfer.wait(),
                Err((err, i8080, buf)) => (Err(err), i8080, buf),
            }
        })
        .unwrap();
    }
}

// ST7789 init from LilyGo factory firmware
// https://github.com/Xinyuan-LilyGO/T-Display-S3/blob/main/examples/tft/Arduino_GFXDemo.ino
fn init_st7789(bus: &mut Bus, delay: &Delay) {
    bus.send(0x11, &[]);
    delay.delay_millis(120);
    bus.send(0x3A, &[0x05]);
    bus.send(0xB2, &[0x0B, 0x0B, 0x00, 0x33, 0x33]);
    bus.send(0xB7, &[0x75]);
    bus.send(0xBB, &[0x28]);
    bus.send(0xC0, &[0x2C]);
    bus.send(0xC2, &[0x01]);
    bus.send(0xC3, &[0x1F]);
    bus.send(0xC6, &[0x13]);
    bus.send(0xD0, &[0xA7]);
    bus.send(0xD0, &[0xA4, 0xA1]);
    bus.send(0xD6, &[0xA1]);
    bus.send(0xE0, &[
        0xF0, 0x05, 0x0A, 0x06, 0x06, 0x03, 0x2B, 0x32,
        0x43, 0x36, 0x11, 0x10, 0x2B, 0x32,
    ]);
    bus.send(0xE1, &[
        0xF0, 0x08, 0x0C, 0x0B, 0x09, 0x24, 0x2B, 0x22,
        0x43, 0x38, 0x15, 0x16, 0x2F, 0x37,
    ]);
    bus.send(0x21, &[]); // INVON
    bus.send(0x13, &[]);
    bus.send(0x36, &[0x60]); // MADCTL landscape
    bus.send(0x29, &[]);
    delay.delay_millis(120);
}

struct EspEmbassyBackend {
    window: Rc<slint::platform::software_renderer::MinimalSoftwareWindow>,
}

impl EspEmbassyBackend {
    fn new(window: Rc<slint::platform::software_renderer::MinimalSoftwareWindow>) -> Self {
        Self { window }
    }
}

impl slint::platform::Platform for EspEmbassyBackend {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }

    fn duration_since_start(&self) -> core::time::Duration {
        embassy_time::Instant::now()
            .duration_since(embassy_time::Instant::from_secs(0))
            .into()
    }
}

#[embassy_executor::task]
async fn graphics_task(
    window: Rc<slint::platform::software_renderer::MinimalSoftwareWindow>,
    ui: slint::Weak<MainWindow>,
    bus_resources: (I8080<'static, Blocking>, DmaTxBuf),
    pixel_buf: &'static mut [Rgb565Pixel],
    button: Input<'static>,
    button2: Input<'static>,
    mut led: Output<'static>,
) {
    info!("Graphics task started");
    let mut bus = Bus { resources: Some(bus_resources) };
    let mut ticker = Ticker::every(Duration::from_millis(50));
    let mut frame_counter = 0u32;
    let buf_len = LCD_H_RES as usize * LCD_V_RES as usize;
    let mut button_pressed = false;
    let mut button2_pressed = false;
    let mut dark_mode = false;
    let mut page: i32 = 0;
    let mut tick: i32 = 0;
    let mut last_frame_ms = embassy_time::Instant::now();

    loop {
        let button_now = button.is_low();

        if !button_now && !button_pressed {
            button_pressed = true;
            esp_println::println!("BTN press");
        } else if button_now && button_pressed {
            button_pressed = false;
            dark_mode = !dark_mode;
            // GPIO2 green LED: active LOW (LOW=ON, HIGH=OFF)
            // LED mirrors dark mode — ON in dark, OFF in light
            if dark_mode {
                led.set_low();
            } else {
                led.set_high();
            }
            if let Some(u) = ui.upgrade() {
                u.set_dark_mode(dark_mode);
                u.set_led_on(dark_mode);
            }
            esp_println::println!("BTN release -> dark_mode={} led={}", dark_mode, dark_mode);
        }

        let button2_now = button2.is_low();
        if !button2_now && !button2_pressed {
            button2_pressed = true;
            esp_println::println!("BTN2 press");
        } else if button2_now && button2_pressed {
            button2_pressed = false;
            page = (page + 1) % 3;
            if let Some(u) = ui.upgrade() {
                u.set_page(page);
            }
            esp_println::println!("BTN2 release -> page={}", page);
        }

        tick = tick.wrapping_add(1);
        if tick % 10 == 0 {
            if let Some(u) = ui.upgrade() {
                u.set_tick(tick);
            }
        }

        slint::platform::update_timers_and_animations();
        window.request_redraw();

        let rendered = window.draw_if_needed(|renderer| {
            renderer.render(pixel_buf, LCD_H_RES as usize);
        });

        if rendered {
            let x_end: u16 = LCD_H_RES - 1;
            let y_start: u16 = ROW_OFFSET;
            let y_end: u16 = ROW_OFFSET + LCD_V_RES - 1;
            bus.send(0x2A, &[0x00, 0x00, (x_end >> 8) as u8, (x_end & 0xFF) as u8]);
            bus.send(0x2B, &[0x00, y_start as u8, 0x00, y_end as u8]);

            let (mut i8080, mut dma_buf) = bus.resources.take().unwrap();
            let total_bytes = buf_len * 2;
            let buf_cap = dma_buf.capacity();
            let mut pixel_offset = 0;

            let first_chunk = total_bytes.min(buf_cap);
            let first_pixels = first_chunk / 2;
            for i in 0..first_pixels {
                let px = pixel_buf[i].0;
                dma_buf.as_mut_slice()[i * 2] = (px & 0xFF) as u8;
                dma_buf.as_mut_slice()[i * 2 + 1] = (px >> 8) as u8;
            }
            dma_buf.set_length(first_chunk);
            let (res, i80, buf) = i8080.send(0x2Cu8, 0u8, dma_buf)
                .expect("DMA send failed")
                .wait();
            if let Err(e) = res {
                esp_println::println!("DMA error frame {} chunk 0: {:?}", frame_counter, e);
            }
            i8080 = i80;
            dma_buf = buf;
            pixel_offset += first_pixels;

            let mut remaining = total_bytes - first_chunk;
            while remaining > 0 {
                let chunk = remaining.min(dma_buf.capacity());
                let pixels = chunk / 2;
                for i in 0..pixels {
                    let px = pixel_buf[pixel_offset + i].0;
                    dma_buf.as_mut_slice()[i * 2] = (px & 0xFF) as u8;
                    dma_buf.as_mut_slice()[i * 2 + 1] = (px >> 8) as u8;
                }
                dma_buf.set_length(chunk);
                let (res, i80, buf) = i8080.send(0x3Cu8, 0u8, dma_buf)
                    .expect("DMA send failed")
                    .wait();
                if let Err(e) = res {
                    esp_println::println!("DMA error frame {} cont: {:?}", frame_counter, e);
                }
                i8080 = i80;
                dma_buf = buf;
                pixel_offset += pixels;
                remaining -= chunk;
            }
            bus.resources = Some((i8080, dma_buf));
            frame_counter = frame_counter.wrapping_add(1);
            if frame_counter % 30 == 0 {
                let now = embassy_time::Instant::now();
                let dt = now.duration_since(last_frame_ms);
                last_frame_ms = now;
                let fps = if dt.as_millis() > 0 { 30000 / dt.as_millis() } else { 0 };
                let free = esp_alloc::HEAP.free();
                esp_println::println!("frame={} fps={} free_heap={}", frame_counter, fps, free);
            }
        }

        ticker.next().await;
    }
}

#[esp_hal_embassy::main]
async fn main(spawner: embassy_executor::Spawner) {
    let peripherals = esp_hal::init(
        esp_hal::Config::default().with_cpu_clock(CpuClock::_240MHz),
    );
    init_logger_from_env();
    info!("LilyGo T-Display S3 — Slint Dark Mode Demo");

    esp_alloc::heap_allocator!(size: 96 * 1024);

    // Pin refs from T-Display-S3 pin_config.h:
    //   GPIO15: PIN_POWER_ON   GPIO38: PIN_LCD_BL
    //   GPIO9:  PIN_LCD_RD     GPIO0: PIN_BUTTON_1
    //   GPIO5:  PIN_LCD_RST   GPIO14: PIN_BUTTON_2
    //
    // GPIO2: Green LED (active LOW — LOW=ON, HIGH=OFF).
    //   Not in LilyGo's official pin_config.h; confirmed via blink test.
    //   The red LED near the USB-C port is a charging indicator driven
    //   by the TP4056 IC — it is NOT user-controllable.
    let _lcd_power = Output::new(peripherals.GPIO15, Level::High, OutputConfig::default());
    let _backlight = Output::new(peripherals.GPIO38, Level::High, OutputConfig::default());
    let _rd = Output::new(peripherals.GPIO9, Level::High, OutputConfig::default());
    let button = Input::new(peripherals.GPIO0, InputConfig::default().with_pull(Pull::Up));
    let button2 = Input::new(peripherals.GPIO14, InputConfig::default().with_pull(Pull::Up));
    let led = Output::new(peripherals.GPIO2, Level::High, OutputConfig::default());

    // I8080 — pin_config.h: GPIO6=CS, GPIO7=DC, GPIO8=WR, GPIO39-48=D0-D7
    let dma_tx_buf = dma_tx_buffer!(32000).unwrap();
    let lcd_cam = LcdCam::new(peripherals.LCD_CAM);
    let tx_pins = TxEightBits::new(
        peripherals.GPIO39, peripherals.GPIO40, peripherals.GPIO41, peripherals.GPIO42,
        peripherals.GPIO45, peripherals.GPIO46, peripherals.GPIO47, peripherals.GPIO48,
    );
    let i8080 = I8080::new(
        lcd_cam.lcd, peripherals.DMA_CH0, tx_pins,
        I8080Config::default().with_frequency(Rate::from_mhz(16)),
    )
    .unwrap()
    .with_ctrl_pins(peripherals.GPIO7, peripherals.GPIO8)
    .with_cs(peripherals.GPIO6);
    info!("I8080 initialized");

    let delay = Delay::new();
    let mut bus = Bus { resources: Some((i8080, dma_tx_buf)) };

    let mut reset = Output::new(peripherals.GPIO5, Level::Low, OutputConfig::default());
    reset.set_low();
    delay.delay_millis(20);
    reset.set_high();
    delay.delay_millis(150);

    init_st7789(&mut bus, &delay);
    info!("ST7789 initialized");

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_hal_embassy::init(timg0.timer0);

    let window = slint::platform::software_renderer::MinimalSoftwareWindow::new(
        slint::platform::software_renderer::RepaintBufferType::ReusedBuffer,
    );
    window.set_size(PhysicalSize::new(LCD_H_RES.into(), LCD_V_RES.into()));
    let backend = Box::new(EspEmbassyBackend::new(window.clone()));
    slint::platform::set_platform(backend).expect("backend already initialized");

    let ui = MainWindow::new().unwrap();
    ui.set_dark_mode(false);
    ui.set_led_on(false);

    let pixel_buf: &'static mut [Rgb565Pixel] = unsafe {
        core::slice::from_raw_parts_mut(core::ptr::addr_of_mut!(PIXEL_BUF) as *mut Rgb565Pixel, PIXEL_COUNT)
    };

    let bus_resources = bus.resources.take().unwrap();
    spawner.spawn(graphics_task(
        window.clone(), ui.as_weak(), bus_resources, pixel_buf, button, button2, led,
    )).ok();

    ui.show().unwrap();
    info!("UI shown");

    let mut ticker = Ticker::every(Duration::from_secs(1));
    loop {
        ticker.next().await;
    }
}
