use core::fmt;
use core::fmt::Write;
use spin::Mutex;
use uart_16550::{backend::PioBackend, Config, Uart16550Tty};

static SERIAL1: Mutex<Option<Uart16550Tty<PioBackend>>> = Mutex::new(None);

#[derive(Clone)]
pub struct Serial {
    port: u16,
}

impl Serial {
    pub fn new(port: u16) -> Self {
        Self { port }
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

pub fn init_global(serial: Serial) {
    let tty = unsafe { Uart16550Tty::new_port(serial.port, Config::default()) }
        .expect("failed to init serial port");
    *SERIAL1.lock() = Some(tty);
    let _ = init_logger();
}

pub fn _print(args: fmt::Arguments) {
    if let Some(ref mut serial) = *SERIAL1.lock() {
        serial.write_fmt(args).expect("serial write failed");
    }
}

// --- Logger ---

static LOGGER: SimpleLogger = SimpleLogger;

fn init_logger() -> Result<(), log::SetLoggerError> {
    log::set_logger(&LOGGER).map(|()| log::set_max_level(log::LevelFilter::Info))
}

struct SimpleLogger;

impl log::Log for SimpleLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Trace
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            _print(format_args!("[{:5}] {}\n", record.level(), record.args()));
        }
    }

    fn flush(&self) {}
}

// --- Macros ---

#[macro_export]
macro_rules! serial_print {
    ($($arg:tt)*) => {
        $crate::serial::_print(format_args!($($arg)*));
    };
}

#[macro_export]
macro_rules! serial_println {
    () => ($crate::serial_print!("\n"));
    ($($arg:tt)*) => ($crate::serial_print!("{}\n", format_args!($($arg)*)));
}
