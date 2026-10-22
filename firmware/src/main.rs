use esp_idf_hal::{
    adc::{
        attenuation,
        oneshot::{config::AdcChannelConfig, AdcChannelDriver, AdcDriver},
    },
    peripherals::Peripherals,
    sys,
    uart::{config::Config as UartConfig, UartDriver, UartTxDriver},
    units::Hertz,
};
use phone_protocol::{
    encode, Decoder, Frame, ANSWER, AUDIO, DIAL, HANGUP, MAX_WIRE, MODEM, PLAY, QUERY,
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

const SAMPLES: usize = 160;
const SAMPLE_US: i64 = 125;
type SharedTx = Arc<Mutex<UartTxDriver<'static>>>;

fn write_all(tx: &SharedTx, bytes: &[u8]) {
    let tx = tx.lock().unwrap();
    let mut sent = 0;
    while sent < bytes.len() {
        match tx.write(&bytes[sent..]) {
            Ok(0) => thread::sleep(Duration::from_millis(1)),
            Ok(n) => sent += n,
            Err(_) => break,
        }
    }
}
fn send_frame(tx: &SharedTx, kind: u8, payload: &[u8]) {
    if let Some(frame) = Frame::new(kind, payload) {
        let mut wire = [0u8; MAX_WIRE];
        let n = encode(&frame, &mut wire);
        write_all(tx, &wire[..n]);
    }
}
fn modem_command(tx: &SharedTx, command: &[u8]) {
    write_all(tx, command);
    write_all(tx, b"\r");
}
fn valid_number(value: &[u8]) -> bool {
    let (first, rest) = if value.first() == Some(&b'+') {
        (true, &value[1..])
    } else {
        (false, value)
    };
    let _ = first;
    (7..=15).contains(&rest.len()) && rest.iter().all(u8::is_ascii_digit)
}
fn modem_reader(
    rx: esp_idf_hal::uart::UartRxDriver<'static>,
    usb: SharedTx,
    active: Arc<AtomicBool>,
) {
    let mut line = [0u8; 128];
    let mut n = 0usize;
    let mut byte = [0u8; 1];
    loop {
        if rx.read(&mut byte, 10).unwrap_or(0) == 0 {
            continue;
        }
        match byte[0] {
            b'\r' => {}
            b'\n' => {
                if n != 0 {
                    if &line[..n] == b"NO CARRIER" || &line[..n] == b"BUSY" {
                        active.store(false, Ordering::Relaxed);
                    }
                    send_frame(&usb, MODEM, &line[..n]);
                    n = 0;
                }
            }
            b => {
                if n < line.len() {
                    line[n] = b;
                    n += 1;
                } else {
                    n = 0;
                }
            }
        }
    }
}
fn playback_loop(rx: mpsc::Receiver<[u8; SAMPLES]>) {
    let cfg = sys::dac_oneshot_config_t {
        chan_id: sys::dac_channel_t_DAC_CHAN_0,
    };
    let mut handle: sys::dac_oneshot_handle_t = std::ptr::null_mut();
    if unsafe { sys::dac_oneshot_new_channel(&cfg, &mut handle) } != 0 {
        return;
    }
    for samples in rx {
        let mut next = unsafe { sys::esp_timer_get_time() };
        for sample in samples {
            unsafe {
                sys::dac_oneshot_output_voltage(handle, sample);
            }
            next += SAMPLE_US;
            while unsafe { sys::esp_timer_get_time() } < next {
                std::hint::spin_loop();
            }
        }
        unsafe {
            sys::dac_oneshot_output_voltage(handle, 128);
        }
    }
    unsafe {
        sys::dac_oneshot_del_channel(handle);
    }
}
fn dispatch(
    frame: Frame,
    modem: &SharedTx,
    active: &AtomicBool,
    playback: &SyncSender<[u8; SAMPLES]>,
) {
    match frame.kind {
        ANSWER => {
            modem_command(modem, b"ATA");
            active.store(true, Ordering::Relaxed);
        }
        HANGUP => {
            active.store(false, Ordering::Relaxed);
            modem_command(modem, b"ATH");
        }
        DIAL if valid_number(frame.payload()) => {
            let mut cmd = [0u8; 20];
            let len = frame.len;
            cmd[..3].copy_from_slice(b"ATD");
            cmd[3..3 + len].copy_from_slice(frame.payload());
            cmd[3 + len] = b';';
            modem_command(modem, &cmd[..4 + len]);
            active.store(true, Ordering::Relaxed);
        }
        QUERY => modem_command(modem, b"AT+CLCC"),
        PLAY if frame.len == SAMPLES => {
            let mut pcm = [128u8; SAMPLES];
            pcm.copy_from_slice(frame.payload());
            let _ = playback.try_send(pcm);
        }
        _ => {}
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    sys::link_patches();
    let peripherals = Peripherals::take().expect("peripherals already taken");
    let usb = UartDriver::new(
        peripherals.uart0,
        peripherals.pins.gpio1,
        peripherals.pins.gpio3,
        None::<esp_idf_hal::gpio::Gpio19>,
        None::<esp_idf_hal::gpio::Gpio22>,
        &UartConfig::new()
            .baudrate(Hertz(460_800))
            .rx_fifo_size(2048)
            .tx_fifo_size(2048),
    )?;
    let modem = UartDriver::new(
        peripherals.uart2,
        peripherals.pins.gpio17,
        peripherals.pins.gpio16,
        None::<esp_idf_hal::gpio::Gpio19>,
        None::<esp_idf_hal::gpio::Gpio22>,
        &UartConfig::new().baudrate(Hertz(9_600)),
    )?;
    let (usb_tx, usb_rx) = usb.into_split();
    let usb_tx = Arc::new(Mutex::new(usb_tx));
    let (modem_tx, modem_rx) = modem.into_split();
    let modem_tx = Arc::new(Mutex::new(modem_tx));
    let active = Arc::new(AtomicBool::new(false));
    let adc_unit = AdcDriver::new(peripherals.adc1)?;
    let adc = AdcChannelDriver::new(
        adc_unit,
        peripherals.pins.gpio34,
        &AdcChannelConfig {
            attenuation: attenuation::DB_12,
            ..AdcChannelConfig::new()
        },
    )?;
    let (play_tx, play_rx) = mpsc::sync_channel(8);
    thread::spawn(move || playback_loop(play_rx));
    {
        let tx = usb_tx.clone();
        let a = active.clone();
        thread::spawn(move || {
            let mut adc = adc;
            let mut samples = [128u8; SAMPLES];
            loop {
                if !a.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
                let mut next = unsafe { sys::esp_timer_get_time() };
                for sample in &mut samples {
                    let raw = adc.read_raw().unwrap_or(2048);
                    *sample = (raw >> 4) as u8;
                    next += SAMPLE_US;
                    while unsafe { sys::esp_timer_get_time() } < next {
                        std::hint::spin_loop();
                    }
                }
                send_frame(&tx, AUDIO, &samples);
            }
        });
    }
    {
        let tx = usb_tx.clone();
        let a = active.clone();
        thread::spawn(move || modem_reader(modem_rx, tx, a));
    }
    modem_command(&modem_tx, b"ATE0");
    modem_command(&modem_tx, b"AT+CLIP=1");
    let mut decoder = Decoder::default();
    let mut input = [0u8; 256];
    loop {
        let n = usb_rx.read(&mut input, 1).unwrap_or(0);
        for &b in &input[..n] {
            if let Some(frame) = decoder.push(b) {
                dispatch(frame, &modem_tx, &active, &play_tx);
            }
        }
    }
}
