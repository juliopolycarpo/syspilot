//! The fan write path of the Nuvoton NCT6776F and NCT6779D. Compiled only
//! with the `fan-control` cargo feature, and tested only against the replay
//! bus: no test of it opens the driver.
//!
//! Register semantics are those of the Linux `drivers/hwmon/nct6775-core.c`
//! (commit 39cd14ca605d, read on 2026-11-23), `NCT6775_REG_FAN_MODE`,
//! `NCT6775_REG_PWM` and `NCT6775_REG_PWM_READ`, used for the `nct6776` and
//! `nct6779` cases of `nct6775_probe`. See `docs/research/fan-control.md` for
//! what that source says, what was checked and what a human must still
//! validate. For duty output `i` (bank `b` = 1, 2, 3, 8, 9 for outputs 1 to
//! 5):
//!
//! - `FAN_MODE` at `b << 8 | 0x02`: bits 6:4 are the control mode, 0 being
//!   manual (`reg_to_pwm_enable`); bits 3:0 hold tolerances. Switching to
//!   manual clears bits 7:4 and keeps bits 3:0 (`store_pwm_enable`).
//! - manual duty at `b << 8 | 0x09`: `0..=255` for `0..=100 %`, the output
//!   while in manual mode (`store_pwm`).
//! - the duty read back at `0x001, 0x003, 0x011, 0x013, 0x015`
//!   (`REG_PWM_READ`).
//!
//! Nothing else is written: not the DC/PWM output type, not the temperature
//! source, not the SmartFan curves, so restoring the two registers resumes
//! the BIOS's own curve.

use std::sync::{Arc, Mutex, TryLockError};
use std::time::{Duration, Instant};

use crate::bus::{
    BusError, BusKind, BusLocks, NCT677X_FAN_MODE_REGISTERS, NCT677X_MANUAL_DUTY_REGISTERS,
    SuperIo, Unlock,
};
use crate::chips::Detected;
use crate::chips::detect::ConfigMode;
use crate::chips::nct67xx::Chip;

use super::board::{BoardChannel, ControlGrant, FAN_CONTROL_CHIPS};
use super::failsafe::{ChannelState, ControlSnapshot, FanChannel};

/// Index of the bank-select register of the hardware monitor.
const BANK_SELECT_INDEX: u8 = 0x4E;
/// Raw duty counts per percent: `0..=255` is `0..=100 %`.
const COUNTS_PER_PERCENT: f64 = 2.55;
/// Bits 6:4 of the fan-mode register: the control mode. 0 is manual.
const MODE_FIELD: u8 = 0x70;
/// Bits 3:0 of the fan-mode register, kept when switching to manual. (Bit 7
/// is cleared with the mode, as `store_pwm_enable` does.)
const MODE_LOW_NIBBLE: u8 = 0x0F;

fn refuse(operation: &str, expected: impl Into<String>, received: impl Into<String>) -> BusError {
    BusError::rejected(format!("nct677x {operation}"), expected, received)
}

/// The raw duty register value for `percent`, 0 to 255, or an error naming
/// the value.
pub fn duty_to_raw(percent: f64) -> Result<u8, BusError> {
    if !percent.is_finite() || !(0.0..=100.0).contains(&percent) {
        return Err(refuse("duty", "a duty within 0..=100 %", format!("{percent}")));
    }
    Ok((percent * COUNTS_PER_PERCENT).round().clamp(0.0, 255.0) as u8)
}

/// The Super I/O handle and bus lock of one NCT677x, shared by its channels.
pub struct Nct677xFanBus {
    io: Box<dyn SuperIo>,
    locks: Arc<dyn BusLocks>,
    base: u16,
    chip: Chip,
}

/// What the bus is doing for one operation: holds the ISA mutex, so the
/// bank-select register the reader's handle also uses cannot be changed under
/// it.
struct Transaction<'a> {
    io: &'a mut dyn SuperIo,
    base: u16,
}

impl Transaction<'_> {
    /// Reads the register at the 16-bit `address`, leaving bank 0 selected.
    fn read(&mut self, address: u16) -> Result<u8, BusError> {
        let [bank, index] = address.to_be_bytes();
        let selected = (|| {
            self.io.hwm_set_index(self.base, BANK_SELECT_INDEX)?;
            self.io.hwm_write_bank(self.base, bank)?;
            self.io.hwm_set_index(self.base, index)?;
            self.io.hwm_read_data(self.base)
        })();
        let restored = self
            .io
            .hwm_set_index(self.base, BANK_SELECT_INDEX)
            .and_then(|()| self.io.hwm_write_bank(self.base, 0));
        let value = selected?;
        restored?;
        Ok(value)
    }

    fn write(&mut self, address: u16, value: u8) -> Result<(), BusError> {
        self.io.hwm_write_fan_register(self.base, address, value)
    }
}

impl Nct677xFanBus {
    /// Opens the write path for a chip the driver detected, on a board whose
    /// entry a human validated.
    ///
    /// `io` must be a fresh handle on the LpcIO module: the driver remembers
    /// the selected slot and the I/O ranges per handle, so this learns them
    /// once, as the read provider does. Takes the ISA mutex itself.
    pub fn open(
        detected: &Detected,
        grant: &ControlGrant<'_>,
        mut io: Box<dyn SuperIo>,
        locks: Arc<dyn BusLocks>,
    ) -> Result<Arc<Mutex<Self>>, BusError> {
        let Detected::Nuvoton { slot, chip, base, .. } = detected else {
            return Err(refuse(
                "open",
                "a detected Nuvoton chip",
                detected.name().unwrap_or_else(|| "a chip that is not a Nuvoton NCT67xx".into()),
            ));
        };
        if *chip != grant.chip() {
            return Err(refuse(
                "open",
                format!("the detected chip to be the board entry's {}", grant.chip().name()),
                chip.name(),
            ));
        }
        if !FAN_CONTROL_CHIPS.contains(chip) {
            return Err(refuse("open", "a chip with a fan-control write path", chip.name()));
        }
        {
            let _bus = locks.acquire(BusKind::Isa)?;
            io.select_slot(*slot)?;
            let mut mode = ConfigMode::enter(io.as_mut(), Unlock::WinbondNuvoton)?;
            let ranges = mode.discover_io_ranges();
            let left = mode.leave();
            ranges?;
            left?;
        }
        Ok(Arc::new(Mutex::new(Self { io, locks, base: *base, chip: *chip })))
    }

    /// Runs `operation` while holding the ISA mutex.
    fn transact<R>(
        &mut self,
        operation: impl FnOnce(&mut Transaction<'_>, Chip) -> Result<R, BusError>,
    ) -> Result<R, BusError> {
        let _bus = self.locks.acquire(BusKind::Isa)?;
        let mut transaction = Transaction { io: self.io.as_mut(), base: self.base };
        operation(&mut transaction, self.chip)
    }
}

/// One duty output of an NCT677x as a [`FanChannel`].
#[derive(Clone)]
pub struct Nct677xChannel {
    bus: Arc<Mutex<Nct677xFanBus>>,
    mode_register: u16,
    duty_register: u16,
    read_back_register: u16,
}

impl Nct677xChannel {
    /// The channel for `channel` of the validated board `bus` was opened for.
    pub fn new(bus: Arc<Mutex<Nct677xFanBus>>, channel: &BoardChannel) -> Result<Self, BusError> {
        let chip = lock(&bus)?.chip;
        let outputs = chip.pwm_channels();
        if channel.pwm == 0 || usize::from(channel.pwm) > outputs {
            return Err(refuse(
                "channel",
                format!("a pwm output within 1..={outputs} on the {}", chip.name()),
                format!("pwm {}", channel.pwm),
            ));
        }
        let index = usize::from(channel.pwm) - 1;
        Ok(Self {
            bus,
            mode_register: NCT677X_FAN_MODE_REGISTERS[index],
            duty_register: NCT677X_MANUAL_DUTY_REGISTERS[index],
            read_back_register: chip.duty_read_registers()[index],
        })
    }

    fn transact<R>(
        &self,
        operation: impl FnOnce(&mut Transaction<'_>, &Self) -> Result<R, BusError>,
    ) -> Result<R, BusError> {
        lock(&self.bus)?.transact(|transaction, _chip| operation(transaction, self))
    }
}

/// How long an operation waits for the bus lock. One operation holds it for
/// a few register accesses, so waiting this long means the holder is stuck,
/// or is this very thread: the panic path restores from a panic hook that may
/// run on the thread that panicked inside an operation, and a lock that
/// waits forever would keep the process alive with the fan in manual mode.
const BUS_LOCK_WAIT: Duration = Duration::from_millis(250);

fn lock(bus: &Mutex<Nct677xFanBus>) -> Result<std::sync::MutexGuard<'_, Nct677xFanBus>, BusError> {
    let deadline = Instant::now() + BUS_LOCK_WAIT;
    loop {
        match bus.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(TryLockError::Poisoned(_)) => {
                return Err(refuse(
                    "bus",
                    "an unpoisoned fan bus",
                    "a lock poisoned by a panic in another operation",
                ));
            }
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(TryLockError::WouldBlock) => {
                return Err(refuse(
                    "bus",
                    format!("the fan bus free within {} ms", BUS_LOCK_WAIT.as_millis()),
                    "a lock still held by another operation",
                ));
            }
        }
    }
}

fn is_manual(mode: u8) -> bool {
    mode & MODE_FIELD == 0
}

impl FanChannel for Nct677xChannel {
    fn read_original(&mut self) -> Result<ControlSnapshot, BusError> {
        self.transact(|bus, this| {
            let duty = bus.read(this.duty_register)?;
            let mode = bus.read(this.mode_register)?;
            // In restore order: the duty first, then the mode.
            Ok(ControlSnapshot {
                registers: vec![(this.duty_register, duty), (this.mode_register, mode)],
            })
        })
    }

    fn read_state(&mut self) -> Result<ChannelState, BusError> {
        self.transact(|bus, this| {
            let mode = bus.read(this.mode_register)?;
            let duty = bus.read(this.read_back_register)?;
            Ok(ChannelState {
                manual: is_manual(mode),
                duty_percent: f64::from(duty) / COUNTS_PER_PERCENT,
            })
        })
    }

    fn take_manual(&mut self, duty_percent: f64) -> Result<(), BusError> {
        let raw = duty_to_raw(duty_percent)?;
        self.transact(|bus, this| {
            // The duty goes first: in the BIOS's automatic mode this register
            // is not the output, and switching the mode before it is set would
            // make the fan jump to whatever it holds.
            bus.write(this.duty_register, raw)?;
            let mode = bus.read(this.mode_register)?;
            bus.write(this.mode_register, mode & MODE_LOW_NIBBLE)
        })
    }

    fn set_duty(&mut self, duty_percent: f64) -> Result<(), BusError> {
        let raw = duty_to_raw(duty_percent)?;
        self.transact(|bus, this| {
            // Only in manual mode: if the BIOS or another tool took the
            // channel back, this must not write a duty nobody follows, and the
            // controller should hear about it.
            let mode = bus.read(this.mode_register)?;
            if !is_manual(mode) {
                return Err(refuse(
                    "set_duty",
                    "the channel in manual mode",
                    format!("fan-mode register {:#06x} = {mode:#04x}", this.mode_register),
                ));
            }
            bus.write(this.duty_register, raw)
        })
    }

    fn restore(&mut self, original: &ControlSnapshot) -> Result<(), BusError> {
        let find = |register: u16| {
            original
                .registers
                .iter()
                .find(|(address, _)| *address == register)
                .map(|&(_, value)| value)
        };
        // Both of this channel's registers, and nothing else, whatever order
        // or content a stored record has.
        let foreign = original
            .registers
            .iter()
            .find(|(address, _)| *address != self.duty_register && *address != self.mode_register);
        if let Some((address, value)) = foreign {
            return Err(refuse(
                "restore",
                format!(
                    "only registers {:#06x} and {:#06x} of this channel",
                    self.duty_register, self.mode_register
                ),
                format!("{value:#04x} for register {address:#06x}"),
            ));
        }
        let (Some(duty), Some(mode)) = (find(self.duty_register), find(self.mode_register)) else {
            return Err(refuse(
                "restore",
                format!(
                    "both registers {:#06x} and {:#06x}",
                    self.duty_register, self.mode_register
                ),
                format!("{:?}", original.registers),
            ));
        };
        self.transact(|bus, this| {
            // The duty while still in manual mode, then the mode that hands
            // the output back to the BIOS.
            bus.write(this.duty_register, duty)?;
            bus.write(this.mode_register, mode)
        })
    }

    fn duplicate(&self) -> Box<dyn FanChannel> {
        // The same bus (and so the same ISA mutex handling and the same
        // allow-listed writes), the same registers.
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{MutexGuard, PoisonError};

    use super::*;
    use crate::bus::FakeBusLocks;
    use crate::bus::replay::{ReplaySuperIo, SuperIoWrite, superio_hwm_key};
    use crate::bus::{Fixture, SuperIoSlot, is_permitted_fan_register};
    use crate::fan::board::BoardCatalog;
    use crate::fan::curve::{Curve, CurvePoint, Readings, TemperatureSource};
    use crate::fan::failsafe::{
        ChannelEvent, ChannelPlan, FailSafePolicy, FanController, OriginalStore, StoreError, Tuning,
    };

    const BASE: u16 = 0x0A20;

    const BOARD: &str = r#"
[[board]]
manufacturer = "ACME"
product = "TEST BOARD"
chip = "NCT6779D"
validated = true
validated-by = "A. Tester"
validated-on = "2026-12-01"

[[board.channel]]
pwm = 1
label = "CPU fan"
min-duty = 20
may-stop = false

[[board.channel]]
pwm = 3
label = "Case fan"
min-duty = 25
may-stop = false
"#;

    /// What the BIOS left: SmartFan IV with tolerance bits set, duty 0x66.
    const BIOS_MODE: u8 = 0x45;
    const BIOS_DUTY: u8 = 0x66;

    fn detected(chip: Chip) -> Detected {
        Detected::Nuvoton {
            slot: SuperIoSlot::Primary,
            chip,
            id_and_revision: 0xC562,
            base: BASE,
            lock_register: 0,
        }
    }

    /// The registers of outputs 1 and 3 of an NCT6779D under BIOS control,
    /// plus registers a fan write must never touch.
    fn chip_fixture() -> Fixture {
        let mut fixture = Fixture::new();
        let mut set = |address: u16, value: u8| {
            fixture.set(
                superio_hwm_key(SuperIoSlot::Primary, (address >> 8) as u8, address as u8),
                u64::from(value),
            );
        };
        for (mode, duty, read_back) in
            [(0x102, 0x109, 0x001), (0x302, 0x309, 0x011), (0x202, 0x209, 0x003)]
        {
            set(mode, BIOS_MODE);
            set(duty, BIOS_DUTY);
            set(read_back, BIOS_DUTY);
        }
        // Output type, temperature source select, a SmartFan target.
        set(0x004, 0x01);
        set(0x621, 0x07);
        set(0x101, 0x37);
        fixture
    }

    struct Rig {
        bus: Arc<Mutex<Nct677xFanBus>>,
        locks: Arc<FakeBusLocks>,
        writes: Arc<Mutex<Vec<SuperIoWrite>>>,
        grant_board: BoardCatalog,
    }

    fn rig() -> Rig {
        let catalog = BoardCatalog::parse("test", BOARD).unwrap();
        let io = ReplaySuperIo::new(chip_fixture());
        let writes = io.write_log();
        let locks = Arc::new(FakeBusLocks::free());
        let grant = catalog.offer("ACME", "TEST BOARD", Chip::Nct6779D).unwrap();
        let bus =
            Nct677xFanBus::open(&detected(Chip::Nct6779D), &grant, Box::new(io), locks.clone())
                .unwrap();
        Rig { bus, locks, writes, grant_board: catalog }
    }

    impl Rig {
        fn channel(&self, pwm: u8) -> Nct677xChannel {
            let grant = self.grant_board.offer("ACME", "TEST BOARD", Chip::Nct6779D).unwrap();
            Nct677xChannel::new(self.bus.clone(), grant.channel(pwm).unwrap()).unwrap()
        }

        /// Every fan-register write so far as `(address, value)`.
        fn fan_writes(&self) -> Vec<(u16, u8)> {
            self.writes
                .lock()
                .unwrap()
                .iter()
                .filter_map(|write| match write {
                    SuperIoWrite::HwmFanRegister { address, value, .. } => Some((*address, *value)),
                    _ => None,
                })
                .collect()
        }

        /// The value of a register in the replayed chip, read through the bus.
        fn register(&self, address: u16) -> u8 {
            let mut bus = self.bus.lock().unwrap();
            bus.transact(|transaction, _| transaction.read(address)).unwrap()
        }
    }

    #[test]
    fn open_learns_the_io_range_and_writes_no_fan_register() {
        let rig = rig();
        let log = rig.writes.lock().unwrap().clone();
        assert_eq!(
            log,
            [
                SuperIoWrite::SelectSlot(SuperIoSlot::Primary),
                SuperIoWrite::Enter(Unlock::WinbondNuvoton),
                SuperIoWrite::DiscoverRanges,
                SuperIoWrite::Exit(Unlock::WinbondNuvoton),
            ]
        );
        assert_eq!(rig.locks.live_guards(), 0);
        assert_eq!(rig.locks.acquired(), [BusKind::Isa]);
    }

    #[test]
    fn open_refuses_a_chip_other_than_the_one_the_board_entry_names() {
        let catalog = BoardCatalog::parse("test", BOARD).unwrap();
        let grant = catalog.offer("ACME", "TEST BOARD", Chip::Nct6779D).unwrap();
        let io = Box::new(ReplaySuperIo::new(chip_fixture()));
        let error = Nct677xFanBus::open(
            &detected(Chip::Nct6776F),
            &grant,
            io,
            Arc::new(FakeBusLocks::free()),
        )
        .err()
        .expect("a different chip should be refused");
        assert_eq!(
            error.to_string(),
            "(nct677x open): expected the detected chip to be the board entry's NCT6779D | received: NCT6776F"
        );
    }

    #[test]
    fn open_refuses_a_chip_that_is_not_a_nuvoton() {
        let catalog = BoardCatalog::parse("test", BOARD).unwrap();
        let grant = catalog.offer("ACME", "TEST BOARD", Chip::Nct6779D).unwrap();
        let unsupported = Detected::Unsupported {
            slot: SuperIoSlot::Primary,
            raw_id: 0x1234,
            name: None,
            reason: "unknown".into(),
        };
        let error = Nct677xFanBus::open(
            &unsupported,
            &grant,
            Box::new(ReplaySuperIo::new(chip_fixture())),
            Arc::new(FakeBusLocks::free()),
        )
        .err()
        .expect("a non-Nuvoton chip should be refused");
        assert!(error.to_string().starts_with("(nct677x open): expected a detected Nuvoton chip"));
    }

    #[test]
    fn open_stops_when_the_isa_bus_is_held_by_another_program() {
        let catalog = BoardCatalog::parse("test", BOARD).unwrap();
        let grant = catalog.offer("ACME", "TEST BOARD", Chip::Nct6779D).unwrap();
        let io = ReplaySuperIo::new(chip_fixture());
        let writes = io.write_log();
        let locks = Arc::new(FakeBusLocks::held_by_another_program(&[BusKind::Isa]));
        let error = Nct677xFanBus::open(&detected(Chip::Nct6779D), &grant, Box::new(io), locks)
            .err()
            .expect("a held bus should be reported");
        assert!(error.is_contended());
        assert!(writes.lock().unwrap().is_empty(), "nothing touched the chip");
    }

    #[test]
    fn a_channel_must_be_one_the_chip_has() {
        let rig = rig();
        let catalog = BoardCatalog::parse("test", BOARD).unwrap();
        let grant = catalog.offer("ACME", "TEST BOARD", Chip::Nct6779D).unwrap();
        let mut beyond = grant.channel(3).unwrap().clone();
        beyond.pwm = 6;
        let error = Nct677xChannel::new(rig.bus.clone(), &beyond).err().unwrap();
        assert_eq!(
            error.to_string(),
            "(nct677x channel): expected a pwm output within 1..=5 on the NCT6779D | received: pwm 6"
        );
        beyond.pwm = 0;
        assert!(Nct677xChannel::new(rig.bus.clone(), &beyond).is_err());
    }

    #[test]
    fn the_channel_registers_follow_the_linux_driver_layout() {
        let rig = rig();
        for (pwm, mode, duty, read_back) in [(1, 0x102, 0x109, 0x001), (3, 0x302, 0x309, 0x011)] {
            let channel = rig.channel(pwm);
            assert_eq!(
                (channel.mode_register, channel.duty_register, channel.read_back_register),
                (mode, duty, read_back),
                "pwm {pwm}"
            );
        }
    }

    #[test]
    fn the_chips_with_a_write_path_fit_the_allow_list() {
        for chip in FAN_CONTROL_CHIPS {
            assert!(chip.pwm_channels() <= NCT677X_FAN_MODE_REGISTERS.len(), "{}", chip.name());
            for index in 0..chip.pwm_channels() {
                assert!(is_permitted_fan_register(NCT677X_FAN_MODE_REGISTERS[index]));
                assert!(is_permitted_fan_register(NCT677X_MANUAL_DUTY_REGISTERS[index]));
            }
        }
    }

    #[test]
    fn reading_the_original_returns_the_duty_then_the_mode_and_writes_nothing() {
        let rig = rig();
        let mut channel = rig.channel(1);
        let original = channel.read_original().unwrap();
        assert_eq!(original.registers, [(0x109, BIOS_DUTY), (0x102, BIOS_MODE)]);
        let third = rig.channel(3).read_original().unwrap();
        assert_eq!(third.registers, [(0x309, BIOS_DUTY), (0x302, BIOS_MODE)]);
        assert!(rig.fan_writes().is_empty());
        assert_eq!(rig.locks.live_guards(), 0);
    }

    #[test]
    fn the_state_reports_the_mode_and_the_duty_read_back() {
        let rig = rig();
        let mut channel = rig.channel(1);
        let state = channel.read_state().unwrap();
        assert!(!state.manual, "SmartFan IV is not manual");
        assert!((state.duty_percent - 40.0).abs() < 0.2, "{}", state.duty_percent);
        assert!(rig.fan_writes().is_empty());
    }

    #[test]
    fn taking_manual_control_writes_the_duty_first_and_keeps_the_low_mode_bits() {
        let rig = rig();
        let mut channel = rig.channel(1);
        channel.take_manual(100.0).unwrap();
        assert_eq!(rig.fan_writes(), [(0x109, 0xFF), (0x102, 0x05)]);
        assert_eq!(rig.register(0x102), 0x05, "bits 7:4 cleared, tolerance bits 3:0 kept");
        assert!(channel.read_state().unwrap().manual);
        assert_eq!(rig.locks.live_guards(), 0);
    }

    #[test]
    fn a_duty_write_changes_only_the_manual_duty_register() {
        let rig = rig();
        let mut channel = rig.channel(1);
        channel.take_manual(40.0).unwrap();
        let before = rig.fan_writes().len();
        channel.set_duty(60.0).unwrap();
        assert_eq!(rig.fan_writes()[before..], [(0x109, 153)]);
        assert_eq!(rig.register(0x109), 153);
    }

    #[test]
    fn the_duty_conversion_covers_the_whole_range() {
        for (percent, raw) in [(0.0, 0), (20.0, 51), (40.0, 102), (60.0, 153), (100.0, 255)] {
            assert_eq!(duty_to_raw(percent).unwrap(), raw, "{percent} %");
        }
        assert_eq!(duty_to_raw(0.1).unwrap(), 0);
        assert_eq!(duty_to_raw(99.9).unwrap(), 255);
    }

    #[test]
    fn a_duty_outside_zero_to_one_hundred_is_refused_before_any_write() {
        let rig = rig();
        let mut channel = rig.channel(1);
        for bad in [-0.1, 100.1, 255.0, f64::NAN, f64::INFINITY] {
            let error = channel.take_manual(bad).unwrap_err();
            assert_eq!(
                error.to_string(),
                format!("(nct677x duty): expected a duty within 0..=100 % | received: {bad}")
            );
            assert!(channel.set_duty(bad).is_err());
        }
        assert!(rig.fan_writes().is_empty());
    }

    #[test]
    fn a_duty_write_is_refused_when_the_channel_is_not_in_manual_mode() {
        let rig = rig();
        let mut channel = rig.channel(1);
        let error = channel.set_duty(50.0).unwrap_err();
        assert_eq!(
            error.to_string(),
            "(nct677x set_duty): expected the channel in manual mode | received: fan-mode register 0x0102 = 0x45"
        );
        assert!(rig.fan_writes().is_empty());
    }

    #[test]
    fn restoring_writes_the_duty_then_the_mode_in_that_order_whatever_the_record_order() {
        let rig = rig();
        let mut channel = rig.channel(1);
        let original = channel.read_original().unwrap();
        channel.take_manual(100.0).unwrap();
        let before = rig.fan_writes().len();

        let reversed =
            ControlSnapshot { registers: original.registers.iter().rev().copied().collect() };
        channel.restore(&reversed).unwrap();
        assert_eq!(rig.fan_writes()[before..], [(0x109, BIOS_DUTY), (0x102, BIOS_MODE)]);
        assert_eq!(rig.register(0x102), BIOS_MODE);
        assert_eq!(rig.register(0x109), BIOS_DUTY);
    }

    #[test]
    fn a_record_with_any_other_register_is_refused_without_a_write() {
        let rig = rig();
        let mut channel = rig.channel(1);
        let foreign = ControlSnapshot {
            registers: vec![(0x109, BIOS_DUTY), (0x102, BIOS_MODE), (0x004, 0x00)],
        };
        let error = channel.restore(&foreign).unwrap_err();
        assert_eq!(
            error.to_string(),
            "(nct677x restore): expected only registers 0x0109 and 0x0102 of this channel | received: 0x00 for register 0x0004"
        );
        // Another channel's registers are as foreign as the output type.
        let other = ControlSnapshot { registers: vec![(0x309, 1), (0x302, 1)] };
        assert!(channel.restore(&other).is_err());
        assert!(rig.fan_writes().is_empty());
    }

    #[test]
    fn a_record_missing_a_register_is_refused_without_a_write() {
        let rig = rig();
        let mut channel = rig.channel(1);
        let partial = ControlSnapshot { registers: vec![(0x109, BIOS_DUTY)] };
        let error = channel.restore(&partial).unwrap_err();
        assert_eq!(
            error.to_string(),
            "(nct677x restore): expected both registers 0x0109 and 0x0102 | received: [(265, 102)]"
        );
        assert!(rig.fan_writes().is_empty());
    }

    #[test]
    fn a_full_session_only_ever_writes_this_channels_two_registers() {
        let rig = rig();
        let mut channel = rig.channel(3);
        let original = channel.read_original().unwrap();
        channel.take_manual(35.0).unwrap();
        channel.set_duty(80.0).unwrap();
        channel.set_duty(100.0).unwrap();
        channel.restore(&original).unwrap();
        let writes = rig.fan_writes();
        assert!(!writes.is_empty());
        for (address, _) in &writes {
            assert!(is_permitted_fan_register(*address), "{address:#06x}");
            assert!([0x309, 0x302].contains(address), "{address:#06x} is not pwm 3's");
        }
        // The registers the fan write must never reach are as recorded.
        assert_eq!(rig.register(0x004), 0x01);
        assert_eq!(rig.register(0x621), 0x07);
        assert_eq!(rig.register(0x101), 0x37);
        // The other channels are untouched.
        assert_eq!(rig.register(0x102), BIOS_MODE);
        assert_eq!(rig.register(0x109), BIOS_DUTY);
    }

    #[test]
    fn every_operation_holds_the_isa_mutex_and_releases_it() {
        let rig = rig();
        let mut channel = rig.channel(1);
        let original = channel.read_original().unwrap();
        channel.read_state().unwrap();
        channel.take_manual(50.0).unwrap();
        channel.set_duty(60.0).unwrap();
        channel.restore(&original).unwrap();
        // Open plus five operations; a failing one releases too.
        assert!(channel.set_duty(60.0).is_err());
        assert_eq!(rig.locks.acquired(), vec![BusKind::Isa; 7]);
        assert_eq!(rig.locks.live_guards(), 0);
    }

    #[test]
    fn a_duplicate_handle_drives_the_same_registers_over_the_same_bus() {
        let rig = rig();
        let mut channel = rig.channel(3);
        let original = channel.read_original().unwrap();
        channel.take_manual(35.0).unwrap();
        let mut duplicate = channel.duplicate();
        duplicate.restore(&original).unwrap();
        assert_eq!(rig.register(0x302), BIOS_MODE, "the duplicate restored pwm 3's mode");
        assert_eq!(rig.register(0x309), BIOS_DUTY);
        assert_eq!(rig.locks.live_guards(), 0);
    }

    #[test]
    fn an_operation_gives_up_on_a_bus_its_own_thread_holds_instead_of_waiting_forever() {
        // The panic hook runs on the thread that panicked inside an operation,
        // so the bus lock it needs may be held by the hook's own thread.
        let rig = rig();
        let mut channel = rig.channel(1);
        let original = channel.read_original().unwrap();
        let held_by_this_thread = rig.bus.lock().unwrap();

        let started = Instant::now();
        let error = channel.restore(&original).unwrap_err().to_string();
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        assert!(
            error.contains("the fan bus free within 250 ms")
                && error.contains("a lock still held by another operation"),
            "{error}"
        );
        drop(held_by_this_thread);
        assert!(rig.fan_writes().is_empty(), "nothing was written while the bus was held");
        channel.restore(&original).unwrap();
    }

    #[test]
    fn a_bus_held_by_another_program_stops_the_operation_before_any_write() {
        // A bus opened, then held elsewhere: the channel reports contention
        // and writes nothing, so the controller retries on the next tick.
        let catalog = BoardCatalog::parse("test", BOARD).unwrap();
        let grant = catalog.offer("ACME", "TEST BOARD", Chip::Nct6779D).unwrap();
        let io = ReplaySuperIo::new(chip_fixture());
        let writes = io.write_log();
        let bus = {
            // Open with a free lock, then swap nothing: contention is modelled
            // by a second bus whose lock is held from the start.
            let locks = Arc::new(FakeBusLocks::free());
            Nct677xFanBus::open(&detected(Chip::Nct6779D), &grant, Box::new(io), locks).unwrap()
        };
        let held = Arc::new(Mutex::new(Nct677xFanBus {
            io: Box::new(ReplaySuperIo::new(chip_fixture())),
            locks: Arc::new(FakeBusLocks::held_by_another_program(&[BusKind::Isa])),
            base: BASE,
            chip: Chip::Nct6779D,
        }));
        let mut channel = Nct677xChannel::new(held, grant.channel(1).unwrap()).unwrap();
        let before = writes.lock().unwrap().len();
        for result in [
            channel.take_manual(50.0),
            channel.set_duty(50.0),
            channel.restore(&ControlSnapshot { registers: vec![(0x109, 1), (0x102, 1)] }),
        ] {
            assert!(result.unwrap_err().is_contended());
        }
        assert!(channel.read_original().unwrap_err().is_contended());
        assert!(channel.read_state().unwrap_err().is_contended());
        assert_eq!(writes.lock().unwrap().len(), before, "no write reached the chip");
        drop(bus);
    }

    // -- With the controller, on the replay bus ----------------------------

    /// An in-memory original store, shared by clones.
    #[derive(Clone, Default)]
    struct MemoryOriginalStore(Arc<Mutex<Vec<(String, ControlSnapshot)>>>);

    impl MemoryOriginalStore {
        /// The records, also after a test thread panicked while holding them,
        /// so that a store method never panics inside its `Result`.
        fn records(&self) -> MutexGuard<'_, Vec<(String, ControlSnapshot)>> {
            self.0.lock().unwrap_or_else(PoisonError::into_inner)
        }
    }

    impl OriginalStore for MemoryOriginalStore {
        fn load(&mut self, channel: &str) -> Result<Option<ControlSnapshot>, StoreError> {
            let records = self.records();
            Ok(records.iter().find(|(id, _)| id == channel).map(|(_, record)| record.clone()))
        }

        fn save(&mut self, channel: &str, original: &ControlSnapshot) -> Result<(), StoreError> {
            self.records().push((channel.to_owned(), original.clone()));
            Ok(())
        }

        fn clear(&mut self, channel: &str) -> Result<(), StoreError> {
            self.records().retain(|(id, _)| id != channel);
            Ok(())
        }

        fn channels(&mut self) -> Result<Vec<String>, StoreError> {
            Ok(self.records().iter().map(|(id, _)| id.clone()).collect())
        }
    }

    fn readings(cpu: Option<f64>) -> Readings {
        [("cpu".to_owned(), cpu)].into_iter().collect()
    }

    fn controller_over(rig: &Rig, store: &MemoryOriginalStore) -> FanController {
        let grant = rig.grant_board.offer("ACME", "TEST BOARD", Chip::Nct6779D).unwrap();
        let point = |temperature, duty| CurvePoint { temperature, duty };
        let curve =
            Curve::new(vec![point(30.0, 20.0), point(60.0, 60.0), point(80.0, 100.0)]).unwrap();
        let plan = ChannelPlan::new(
            grant.channel(1).unwrap(),
            curve,
            TemperatureSource::single("cpu"),
            Tuning { hysteresis: 0.0, max_rise_per_second: None, max_fall_per_second: None },
        )
        .unwrap();
        FanController::new(
            FailSafePolicy::default(),
            Box::new(store.clone()),
            vec![(plan, Box::new(rig.channel(1)))],
        )
        .unwrap()
    }

    #[test]
    fn the_controller_takes_a_replayed_chip_over_fails_safe_and_hands_it_back() {
        let rig = rig();
        let store = MemoryOriginalStore::default();
        let mut controller = controller_over(&rig, &store);

        // Three good ticks take control at the curve's duty for 45 °C: 40 %.
        let mut events = Vec::new();
        for _ in 0..3 {
            events.extend(controller.tick(&readings(Some(45.0)), 1.0));
        }
        assert!(
            matches!(events[0].event, ChannelEvent::TookControl { duty } if (duty - 40.0).abs() < 1e-9)
        );
        assert_eq!(rig.register(0x102), 0x05);
        assert_eq!(rig.register(0x109), 102);
        assert_eq!(store.0.lock().unwrap().len(), 1, "the original is stored");

        // The sensor fails: 100 %.
        controller.tick(&readings(None), 1.0);
        assert_eq!(rig.register(0x109), 0xFF);

        // Stop: the BIOS registers are back and the record is gone.
        controller.stop();
        assert_eq!(rig.register(0x102), BIOS_MODE);
        assert_eq!(rig.register(0x109), BIOS_DUTY);
        assert!(store.0.lock().unwrap().is_empty());
        assert_eq!(rig.locks.live_guards(), 0);
    }

    #[test]
    fn dropping_the_controller_returns_a_replayed_chip_to_the_bios() {
        let rig = rig();
        let store = MemoryOriginalStore::default();
        let mut controller = controller_over(&rig, &store);
        for _ in 0..3 {
            controller.tick(&readings(Some(45.0)), 1.0);
        }
        assert_eq!(rig.register(0x102), 0x05);
        drop(controller);
        assert_eq!(rig.register(0x102), BIOS_MODE);
        assert_eq!(rig.register(0x109), BIOS_DUTY);
    }

    #[test]
    fn a_restart_on_a_replayed_chip_restores_the_stored_bios_mode() {
        let rig = rig();
        let store = MemoryOriginalStore::default();
        let mut crashed = controller_over(&rig, &store);
        for _ in 0..3 {
            crashed.tick(&readings(Some(45.0)), 1.0);
        }
        std::mem::forget(crashed);
        assert_eq!(rig.register(0x102), 0x05, "the crash left manual mode");

        let mut restarted = controller_over(&rig, &store);
        restarted.recover();
        assert_eq!(rig.register(0x102), BIOS_MODE);
        assert_eq!(rig.register(0x109), BIOS_DUTY);
    }
}
