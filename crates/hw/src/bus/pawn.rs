//! The bus traits implemented on PawnIO modules.
//!
//! Each type wraps an [`Executor`] that already holds the right module
//! (`IntelMSR` or `AMDFamily17` for [`PawnMsr`], `AMDFamily17` for
//! [`PawnSmn`], `LpcIO` for [`PawnSuperIo`], `SmbusI801` or `SmbusPIIX4` for
//! [`PawnSmbus`], `IntelPCHThermal` for [`PawnPchThermal`]). Tests drive them with a scripted executor and assert the
//! exact driver calls, which is how the "reads only" rule is checked at the
//! lowest level.

use std::sync::Arc;

use syspilot_pawnio::Executor;
use windows_sys::Win32::System::SystemInformation::GROUP_AFFINITY;
use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadGroupAffinity};

#[cfg(feature = "spd-paging")]
use super::SPD_PAGE_SELECT_ADDRESSES;
#[cfg(feature = "fan-control")]
use super::is_permitted_fan_register;
use super::{
    BusError, LogicalCpu, Msr, PchReading, PchRoute, PchThermal, SMN_THERMAL_BLOCK, Smbus, Smn,
    SuperIo, SuperIoSlot, Unlock, is_permitted_bank, is_permitted_smn_address,
};

/// Pins the calling thread to one logical processor and restores the
/// previous affinity on drop.
struct ThreadPin {
    previous: GROUP_AFFINITY,
}

impl ThreadPin {
    fn to(cpu: LogicalCpu) -> Result<Self, BusError> {
        let target = GROUP_AFFINITY { Mask: cpu.mask(), Group: cpu.group, Reserved: [0; 3] };
        let mut previous = GROUP_AFFINITY { Mask: 0, Group: 0, Reserved: [0; 3] };
        // SAFETY: the pseudo handle from `GetCurrentThread` is always valid;
        // both pointers refer to live `GROUP_AFFINITY` values.
        let pinned = unsafe { SetThreadGroupAffinity(GetCurrentThread(), &target, &mut previous) };
        if pinned == 0 {
            return Err(BusError::Os {
                operation: "SetThreadGroupAffinity",
                win32: std::io::Error::last_os_error().raw_os_error().unwrap_or(0) as u32,
            });
        }
        Ok(Self { previous })
    }
}

impl Drop for ThreadPin {
    fn drop(&mut self) {
        // SAFETY: as in `to`; `previous` is the affinity the OS reported when
        // the pin was set.
        let restored = unsafe {
            SetThreadGroupAffinity(GetCurrentThread(), &self.previous, std::ptr::null_mut())
        };
        debug_assert_ne!(restored, 0, "restoring the thread affinity failed");
    }
}

/// `ioctl_read_msr` with the thread pinned to the target processor.
pub struct PawnMsr {
    module: Arc<dyn Executor>,
}

impl PawnMsr {
    /// Wraps a handle holding `IntelMSR` or `AMDFamily17`.
    pub fn new(module: Arc<dyn Executor>) -> Self {
        Self { module }
    }
}

impl Msr for PawnMsr {
    fn read(&mut self, cpu: LogicalCpu, register: u32) -> Result<u64, BusError> {
        let _pin = ThreadPin::to(cpu)?;
        Ok(self.module.execute_scalar("ioctl_read_msr", &[u64::from(register)])?)
    }
}

/// `ioctl_read_smn` of `AMDFamily17`.
pub struct PawnSmn {
    module: Arc<dyn Executor>,
}

impl PawnSmn {
    /// Wraps a handle holding `AMDFamily17`.
    pub fn new(module: Arc<dyn Executor>) -> Self {
        Self { module }
    }
}

impl Smn for PawnSmn {
    fn read(&mut self, address: u32) -> Result<u32, BusError> {
        if !is_permitted_smn_address(address) {
            return Err(BusError::rejected(
                "ioctl_read_smn",
                format!(
                    "an address in the thermal block {:#x}..={:#x}",
                    SMN_THERMAL_BLOCK.start(),
                    SMN_THERMAL_BLOCK.end()
                ),
                format!("{address:#x}"),
            ));
        }
        let value = self.module.execute_scalar("ioctl_read_smn", &[u64::from(address)])?;
        u32::try_from(value).map_err(|_| {
            BusError::rejected(
                "ioctl_read_smn",
                "a 32-bit register value",
                format!("{value:#x} for SMN address {address:#x}"),
            )
        })
    }
}

/// The `LpcIO` module as a [`SuperIo`].
pub struct PawnSuperIo {
    module: Arc<dyn Executor>,
    slot: Option<SuperIoSlot>,
    /// Set by `hwm_set_index(base, 0x4E)`, consumed by `hwm_write_bank`;
    /// every other operation clears it, so a bank write can only directly
    /// follow the bank-select index.
    bank_select_armed: Option<u16>,
}

/// Index of the Nuvoton bank-select register in the hardware monitor.
const NUVOTON_BANK_SELECT_INDEX: u8 = 0x4E;
/// Logical-device-select configuration register.
const LOGICAL_DEVICE_SELECT_REGISTER: u64 = 0x07;
/// ITE leaves configuration mode by writing this value to register `0x02`.
const ITE_EXIT_REGISTER: u64 = 0x02;
const ITE_EXIT_VALUE: u64 = 0x02;

impl PawnSuperIo {
    /// Wraps a handle holding `LpcIO`. Call [`SuperIo::select_slot`] first.
    pub fn new(module: Arc<dyn Executor>) -> Self {
        Self { module, slot: None, bank_select_armed: None }
    }

    fn selected_slot(&self, operation: &str) -> Result<SuperIoSlot, BusError> {
        self.slot.ok_or_else(|| {
            BusError::rejected(
                operation,
                "select_slot before any other operation",
                "no slot selected",
            )
        })
    }

    fn write_port(&self, port: u16, value: u8) -> Result<(), BusError> {
        Ok(self.module.execute("ioctl_pio_outb", &[u64::from(port), u64::from(value)], &mut [])?)
    }

    fn read_port(&self, port: u16) -> Result<u8, BusError> {
        let value = self.module.execute_scalar("ioctl_pio_inb", &[u64::from(port)])?;
        Ok(value as u8)
    }
}

impl SuperIo for PawnSuperIo {
    fn select_slot(&mut self, slot: SuperIoSlot) -> Result<(), BusError> {
        self.bank_select_armed = None;
        let cell = match slot {
            SuperIoSlot::Primary => 0,
            SuperIoSlot::Secondary => 1,
        };
        self.module.execute("ioctl_select_slot", &[cell], &mut [])?;
        self.slot = Some(slot);
        Ok(())
    }

    fn enter_config_mode(&mut self, unlock: Unlock) -> Result<(), BusError> {
        self.bank_select_armed = None;
        let slot = self.selected_slot("enter_config_mode")?;
        let port = slot.register_port();
        match unlock {
            Unlock::WinbondNuvoton => {
                self.write_port(port, 0x87)?;
                self.write_port(port, 0x87)
            }
            Unlock::Ite => {
                let last = match slot {
                    SuperIoSlot::Primary => 0x55,
                    SuperIoSlot::Secondary => 0xAA,
                };
                for byte in [0x87, 0x01, 0x55, last] {
                    self.write_port(port, byte)?;
                }
                Ok(())
            }
        }
    }

    fn exit_config_mode(&mut self, unlock: Unlock) -> Result<(), BusError> {
        self.bank_select_armed = None;
        let slot = self.selected_slot("exit_config_mode")?;
        match unlock {
            Unlock::WinbondNuvoton => self.write_port(slot.register_port(), 0xAA),
            Unlock::Ite => Ok(self.module.execute(
                "ioctl_superio_outb",
                &[ITE_EXIT_REGISTER, ITE_EXIT_VALUE],
                &mut [],
            )?),
        }
    }

    fn select_logical_device(&mut self, device: u8) -> Result<(), BusError> {
        self.bank_select_armed = None;
        self.selected_slot("select_logical_device")?;
        Ok(self.module.execute(
            "ioctl_superio_outb",
            &[LOGICAL_DEVICE_SELECT_REGISTER, u64::from(device)],
            &mut [],
        )?)
    }

    fn config_read(&mut self, register: u8) -> Result<u8, BusError> {
        self.bank_select_armed = None;
        self.selected_slot("config_read")?;
        let value = self.module.execute_scalar("ioctl_superio_inb", &[u64::from(register)])?;
        Ok(value as u8)
    }

    fn discover_io_ranges(&mut self) -> Result<(), BusError> {
        self.bank_select_armed = None;
        self.selected_slot("discover_io_ranges")?;
        Ok(self.module.execute("ioctl_find_bars", &[], &mut [])?)
    }

    fn hwm_set_index(&mut self, base: u16, index: u8) -> Result<(), BusError> {
        self.bank_select_armed = None;
        self.write_port(base.wrapping_add(5), index)?;
        if index == NUVOTON_BANK_SELECT_INDEX {
            self.bank_select_armed = Some(base);
        }
        Ok(())
    }

    fn hwm_write_bank(&mut self, base: u16, bank: u8) -> Result<(), BusError> {
        let armed = self.bank_select_armed.take();
        if armed != Some(base) {
            return Err(BusError::rejected(
                "hwm_write_bank",
                format!(
                    "a write directly after selecting index {NUVOTON_BANK_SELECT_INDEX:#04x} at base {base:#06x}"
                ),
                "no matching bank-select index was set",
            ));
        }
        if !is_permitted_bank(bank) {
            return Err(BusError::rejected(
                "hwm_write_bank",
                "a bank number 0..=0x0f, or 0x80 for the high byte of 16-bit registers",
                format!("bank {bank:#x}"),
            ));
        }
        self.write_port(base.wrapping_add(6), bank)
    }

    fn hwm_read_index(&mut self, base: u16) -> Result<u8, BusError> {
        self.bank_select_armed = None;
        self.read_port(base.wrapping_add(5))
    }

    fn hwm_read_data(&mut self, base: u16) -> Result<u8, BusError> {
        self.bank_select_armed = None;
        self.read_port(base.wrapping_add(6))
    }

    #[cfg(feature = "fan-control")]
    fn hwm_write_fan_register(
        &mut self,
        base: u16,
        address: u16,
        value: u8,
    ) -> Result<(), BusError> {
        self.bank_select_armed = None;
        if !is_permitted_fan_register(address) {
            return Err(BusError::rejected(
                "hwm_write_fan_register",
                "a fan-mode or manual-duty register of the NCT6776F or NCT6779D",
                format!("address {address:#06x}"),
            ));
        }
        let [bank, index] = address.to_be_bytes();
        let (index_port, data_port) = (base.wrapping_add(5), base.wrapping_add(6));
        let written = (|| {
            self.write_port(index_port, NUVOTON_BANK_SELECT_INDEX)?;
            self.write_port(data_port, bank)?;
            self.write_port(index_port, index)?;
            self.write_port(data_port, value)
        })();
        // Leave bank 0 selected, as the readers expect, even when a write
        // failed. Selecting the bank register first means this cannot land a
        // data write on the register the failed sequence left indexed.
        let restored = self
            .write_port(index_port, NUVOTON_BANK_SELECT_INDEX)
            .and_then(|()| self.write_port(data_port, 0));
        written.and(restored)
    }
}

/// `ioctl_smbus_xfer` of `SmbusI801` or `SmbusPIIX4` as an [`Smbus`].
pub struct PawnSmbus {
    module: Arc<dyn Executor>,
}

const SMBUS_READ: u64 = 1;
#[cfg(feature = "spd-paging")]
const SMBUS_WRITE: u64 = 0;
#[cfg(feature = "spd-paging")]
const PROTOCOL_BYTE: u64 = 1;
const PROTOCOL_BYTE_DATA: u64 = 2;
const PROTOCOL_WORD_DATA: u64 = 3;
/// `ioctl_smbus_xfer` has a fixed signature: nine input cells (address,
/// direction, command, protocol, five data cells) and five output cells. The
/// driver rejects any other size with `STATUS_INVALID_PARAMETER` before it
/// touches the bus.
const XFER_INPUT_CELLS: usize = 9;
const XFER_OUTPUT_CELLS: usize = 5;

impl PawnSmbus {
    /// Wraps a handle holding `SmbusI801` or `SmbusPIIX4`.
    pub fn new(module: Arc<dyn Executor>) -> Self {
        Self { module }
    }

    /// One transfer, given as address, direction, command and protocol; the
    /// cells after `protocol` carry no data for the protocols this layer uses.
    fn transfer(&self, head: [u64; 4]) -> Result<[u64; XFER_OUTPUT_CELLS], BusError> {
        let mut request = [0u64; XFER_INPUT_CELLS];
        request[..4].copy_from_slice(&head);
        let mut reply = [0u64; XFER_OUTPUT_CELLS];
        self.module.execute("ioctl_smbus_xfer", &request, &mut reply)?;
        Ok(reply)
    }

    /// A read. The only transfer in the default build: no direction is
    /// passed around, so no write can be built from here.
    fn read(&self, address: u8, command: u8, protocol: u64) -> Result<u64, BusError> {
        Ok(self.transfer([u64::from(address), SMBUS_READ, u64::from(command), protocol])?[0])
    }

    /// "Write byte" with value 0 to a page-select device, which acts on its
    /// own address and ignores the data. The one SMBus write SysPilot can
    /// make, and the assert keeps it aimed at `0x36`/`0x37`: a write to any
    /// other address could reconfigure a clock generator or a voltage
    /// regulator.
    #[cfg(feature = "spd-paging")]
    fn write_page_select(&self, target: u8) -> Result<(), BusError> {
        assert!(
            SPD_PAGE_SELECT_ADDRESSES.contains(&target),
            "(smbus write): expected target 0x36 or 0x37 | received: {target:#04x}"
        );
        self.transfer([u64::from(target), SMBUS_WRITE, 0, PROTOCOL_BYTE])?;
        Ok(())
    }
}

impl Smbus for PawnSmbus {
    fn read_byte_data(&mut self, address: u8, command: u8) -> Result<u8, BusError> {
        let value = self.read(address, command, PROTOCOL_BYTE_DATA)?;
        u8::try_from(value).map_err(|_| {
            BusError::rejected(
                "ioctl_smbus_xfer byte read",
                "a value that fits one byte",
                format!("{value:#x}"),
            )
        })
    }

    fn read_word_data(&mut self, address: u8, command: u8) -> Result<u16, BusError> {
        let value = self.read(address, command, PROTOCOL_WORD_DATA)?;
        u16::try_from(value).map_err(|_| {
            BusError::rejected(
                "ioctl_smbus_xfer word read",
                "a value that fits one word",
                format!("{value:#x}"),
            )
        })
    }

    #[cfg(feature = "spd-paging")]
    fn select_spd_page(&mut self, page: u8) -> Result<(), BusError> {
        let target = *SPD_PAGE_SELECT_ADDRESSES.get(usize::from(page)).ok_or_else(|| {
            BusError::rejected("select_spd_page", "page 0 or 1", format!("page {page}"))
        })?;
        self.write_page_select(target)
    }
}

/// `ioctl_read_raw` of `IntelPCHThermal` as a [`PchThermal`].
///
/// The module has no write function, so this is the only call the type makes.
pub struct PawnPchThermal {
    module: Arc<dyn Executor>,
}

/// `ioctl_read_raw` has a fixed signature: no input cells and two output
/// cells, the raw register and the route.
const PCH_READ_OUTPUT_CELLS: usize = 2;

impl PawnPchThermal {
    /// Wraps a handle holding `IntelPCHThermal`.
    pub fn new(module: Arc<dyn Executor>) -> Self {
        Self { module }
    }
}

impl PchThermal for PawnPchThermal {
    fn read_raw(&mut self) -> Result<PchReading, BusError> {
        let mut cells = [0u64; PCH_READ_OUTPUT_CELLS];
        self.module.execute("ioctl_read_raw", &[], &mut cells)?;
        let [raw, route] = cells;
        let route = PchRoute::from_cell(route).ok_or_else(|| {
            BusError::rejected(
                "ioctl_read_raw",
                "route 1 (PCI thermal BAR0) or 2 (fixed PWRM window)",
                format!("route {route}"),
            )
        })?;
        let raw = u32::try_from(raw).map_err(|_| {
            BusError::rejected(
                "ioctl_read_raw",
                "a register value that fits 32 bits",
                format!("{raw:#x}"),
            )
        })?;
        Ok(PchReading { raw, route })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syspilot_pawnio::{Error as DriverError, RecordedCall, ScriptedExecutor};

    fn scripted() -> (Arc<ScriptedExecutor>, Arc<dyn Executor>) {
        let scripted = Arc::new(ScriptedExecutor::new());
        scripted.repeat_when_empty(Ok(vec![0]));
        let dynamic: Arc<dyn Executor> = scripted.clone();
        (scripted, dynamic)
    }

    fn call(function: &str, input: &[u64], output_cells: usize) -> RecordedCall {
        RecordedCall { function: function.into(), input: input.to_vec(), output_cells }
    }

    /// The first logical processor the test thread may run on.
    fn allowed_cpu() -> LogicalCpu {
        let mut affinity = GROUP_AFFINITY { Mask: 0, Group: 0, Reserved: [0; 3] };
        // SAFETY: valid pseudo handle and out-pointer.
        let ok = unsafe {
            windows_sys::Win32::System::Threading::GetThreadGroupAffinity(
                GetCurrentThread(),
                &mut affinity,
            )
        };
        assert_ne!(ok, 0, "GetThreadGroupAffinity failed");
        LogicalCpu { group: affinity.Group, index: affinity.Mask.trailing_zeros() as u8 }
    }

    fn current_processor() -> (u16, u32) {
        let mut number = windows_sys::Win32::System::Kernel::PROCESSOR_NUMBER {
            Group: 0,
            Number: 0,
            Reserved: 0,
        };
        // SAFETY: valid out-pointer.
        unsafe { windows_sys::Win32::System::Threading::GetCurrentProcessorNumberEx(&mut number) };
        (number.Group, u32::from(number.Number))
    }

    #[test]
    fn msr_read_sends_the_register_as_one_cell() {
        let (scripted, module) = scripted();
        scripted.reply_ok(&[0x0064_0E00]);
        let mut msr = PawnMsr::new(module);
        assert_eq!(msr.read(allowed_cpu(), 0x1A2), Ok(0x0064_0E00));
        assert_eq!(scripted.calls(), [call("ioctl_read_msr", &[0x1A2], 1)]);
    }

    #[test]
    fn msr_read_runs_on_the_requested_processor_and_restores_affinity() {
        let cpu = allowed_cpu();
        let before = {
            let mut affinity = GROUP_AFFINITY { Mask: 0, Group: 0, Reserved: [0; 3] };
            // SAFETY: valid pseudo handle and out-pointer.
            unsafe {
                windows_sys::Win32::System::Threading::GetThreadGroupAffinity(
                    GetCurrentThread(),
                    &mut affinity,
                )
            };
            affinity.Mask
        };
        let pin = ThreadPin::to(cpu).unwrap();
        // The scheduler moves the thread on its next slice, so give it a
        // moment and sample a few times.
        let mut seen = Vec::new();
        for _ in 0..5 {
            std::thread::yield_now();
            seen.push(current_processor());
        }
        assert!(
            seen.iter()
                .all(|&(group, number)| group == cpu.group && number == u32::from(cpu.index)),
            "(thread pin): expected processor {}:{} | received: {seen:?}",
            cpu.group,
            cpu.index
        );
        drop(pin);
        let mut after = GROUP_AFFINITY { Mask: 0, Group: 0, Reserved: [0; 3] };
        // SAFETY: valid pseudo handle and out-pointer.
        unsafe {
            windows_sys::Win32::System::Threading::GetThreadGroupAffinity(
                GetCurrentThread(),
                &mut after,
            )
        };
        assert_eq!(after.Mask, before, "(thread pin): expected the previous affinity back");
    }

    #[test]
    fn pinning_to_a_processor_that_does_not_exist_is_an_os_error() {
        let (scripted, module) = scripted();
        let mut msr = PawnMsr::new(module);
        let error = msr.read(LogicalCpu { group: 63, index: 63 }, 0x1A2).unwrap_err();
        assert!(
            matches!(error, BusError::Os { operation: "SetThreadGroupAffinity", .. }),
            "{error}"
        );
        assert!(scripted.calls().is_empty(), "the driver must not be called unpinned");
    }

    #[test]
    fn msr_denial_keeps_the_driver_error() {
        let (scripted, module) = scripted();
        scripted.reply_err(DriverError::ArgumentDenied {
            function: "ioctl_read_msr".into(),
            argument: Some(0x10),
        });
        let mut msr = PawnMsr::new(module);
        let error = msr.read(allowed_cpu(), 0x10).unwrap_err();
        assert!(matches!(error, BusError::Driver(DriverError::ArgumentDenied { .. })));
    }

    #[test]
    fn smn_read_returns_32_bits_and_rejects_wider_values() {
        let (scripted, module) = scripted();
        scripted.reply_ok(&[0x0123_4567]).reply_ok(&[0x1_0000_0000]);
        let mut smn = PawnSmn::new(module);
        assert_eq!(smn.read(0x0005_9800), Ok(0x0123_4567));
        let error = smn.read(0x0005_9800).unwrap_err();
        assert_eq!(
            error.to_string(),
            "(ioctl_read_smn): expected a 32-bit register value | received: 0x100000000 for SMN \
             address 0x59800"
        );
        assert_eq!(scripted.calls()[0], call("ioctl_read_smn", &[0x0005_9800], 1));
    }

    #[test]
    fn smn_reads_outside_the_thermal_block_never_reach_the_driver() {
        let (scripted, module) = scripted();
        let mut smn = PawnSmn::new(module);
        for address in [0x0005_97FF, 0x0005_9C00, 0x0000_0000, 0x0003_0000, u32::MAX] {
            let error = smn.read(address).unwrap_err();
            assert_eq!(
                error.to_string(),
                format!(
                    "(ioctl_read_smn): expected an address in the thermal block \
                     0x59800..=0x59bff | received: {address:#x}"
                )
            );
        }
        assert_eq!(scripted.calls(), vec![], "no call was made");
    }

    #[test]
    fn the_thermal_block_covers_every_ccd_register_the_k10temp_layouts_use() {
        // Tctl, and the last CCD register of the widest layout (family 0x1A
        // models 0x00..=0x1F: offset 0x1F0, 16 CCDs; family 0x19 model 0x60:
        // offset 0x308, 12 slots).
        for address in
            [0x59800, 0x59800 + 0x154, 0x59800 + 0x1F0 + 15 * 4, 0x59800 + 0x308 + 11 * 4]
        {
            assert!(is_permitted_smn_address(address), "{address:#x}");
        }
    }

    #[test]
    fn super_io_refuses_every_operation_before_a_slot_is_selected() {
        let (scripted, module) = scripted();
        let mut chip = PawnSuperIo::new(module);
        assert!(chip.enter_config_mode(Unlock::WinbondNuvoton).is_err());
        assert!(chip.exit_config_mode(Unlock::Ite).is_err());
        assert!(chip.select_logical_device(0x0B).is_err());
        assert!(chip.config_read(0x20).is_err());
        assert!(chip.discover_io_ranges().is_err());
        assert!(scripted.calls().is_empty());
    }

    #[test]
    fn nuvoton_unlock_writes_0x87_twice_and_exit_writes_0xaa_to_the_register_port() {
        let (scripted, module) = scripted();
        let mut chip = PawnSuperIo::new(module);
        chip.select_slot(SuperIoSlot::Secondary).unwrap();
        chip.enter_config_mode(Unlock::WinbondNuvoton).unwrap();
        chip.exit_config_mode(Unlock::WinbondNuvoton).unwrap();
        assert_eq!(
            scripted.calls(),
            [
                call("ioctl_select_slot", &[1], 0),
                call("ioctl_pio_outb", &[0x4E, 0x87], 0),
                call("ioctl_pio_outb", &[0x4E, 0x87], 0),
                call("ioctl_pio_outb", &[0x4E, 0xAA], 0),
            ]
        );
    }

    #[test]
    fn ite_unlock_ends_with_the_slot_specific_byte_and_exit_writes_register_two() {
        let (scripted, module) = scripted();
        let mut chip = PawnSuperIo::new(module);
        chip.select_slot(SuperIoSlot::Primary).unwrap();
        chip.enter_config_mode(Unlock::Ite).unwrap();
        chip.select_slot(SuperIoSlot::Secondary).unwrap();
        chip.enter_config_mode(Unlock::Ite).unwrap();
        chip.exit_config_mode(Unlock::Ite).unwrap();
        let calls = scripted.calls();
        let primary: Vec<_> = calls[1..5].iter().map(|c| c.input.clone()).collect();
        assert_eq!(primary, [[0x2E, 0x87], [0x2E, 0x01], [0x2E, 0x55], [0x2E, 0x55]]);
        let secondary: Vec<_> = calls[6..10].iter().map(|c| c.input.clone()).collect();
        assert_eq!(secondary, [[0x4E, 0x87], [0x4E, 0x01], [0x4E, 0x55], [0x4E, 0xAA]]);
        assert_eq!(calls[10], call("ioctl_superio_outb", &[0x02, 0x02], 0));
    }

    #[test]
    fn logical_device_select_writes_register_seven_and_nothing_else() {
        let (scripted, module) = scripted();
        let mut chip = PawnSuperIo::new(module);
        chip.select_slot(SuperIoSlot::Primary).unwrap();
        chip.select_logical_device(0x0B).unwrap();
        assert_eq!(scripted.calls()[1], call("ioctl_superio_outb", &[0x07, 0x0B], 0));
    }

    #[test]
    fn config_reads_and_discovery_use_read_functions_and_find_bars() {
        let (scripted, module) = scripted();
        scripted.reply_ok(&[]).reply_ok(&[0xC5]).reply_ok(&[0x60]).reply_ok(&[]);
        let mut chip = PawnSuperIo::new(module);
        chip.select_slot(SuperIoSlot::Primary).unwrap();
        assert_eq!(chip.config_read(0x20), Ok(0xC5));
        assert_eq!(chip.config_read(0x21), Ok(0x60));
        chip.discover_io_ranges().unwrap();
        let calls = scripted.calls();
        assert_eq!(calls[1], call("ioctl_superio_inb", &[0x20], 1));
        assert_eq!(calls[2], call("ioctl_superio_inb", &[0x21], 1));
        assert_eq!(calls[3], call("ioctl_find_bars", &[], 0));
    }

    #[test]
    fn config_word_is_big_endian() {
        let (scripted, module) = scripted();
        scripted.reply_ok(&[]).reply_ok(&[0x86]).reply_ok(&[0x28]);
        let mut chip = PawnSuperIo::new(module);
        chip.select_slot(SuperIoSlot::Primary).unwrap();
        assert_eq!(chip.config_read_word(0x20), Ok(0x8628));
    }

    #[test]
    fn a_bank_write_is_only_possible_right_after_the_bank_select_index() {
        let (scripted, module) = scripted();
        let mut chip = PawnSuperIo::new(module);
        chip.select_slot(SuperIoSlot::Primary).unwrap();

        // Without the index: refused, nothing is written.
        let error = chip.hwm_write_bank(0x0290, 1).unwrap_err();
        assert!(matches!(error, BusError::Rejected { .. }), "{error}");

        // With another index: refused.
        chip.hwm_set_index(0x0290, 0x73).unwrap();
        assert!(chip.hwm_write_bank(0x0290, 1).is_err());

        // With the bank-select index at another base: refused.
        chip.hwm_set_index(0x0290, 0x4E).unwrap();
        assert!(chip.hwm_write_bank(0x0A20, 1).is_err());

        // The index, then an unrelated read, then the write: refused.
        chip.hwm_set_index(0x0290, 0x4E).unwrap();
        chip.hwm_read_data(0x0290).unwrap();
        assert!(chip.hwm_write_bank(0x0290, 1).is_err());

        // The allowed sequence.
        chip.hwm_set_index(0x0290, 0x4E).unwrap();
        chip.hwm_write_bank(0x0290, 1).unwrap();
        // The permission is consumed by one write.
        assert!(chip.hwm_write_bank(0x0290, 1).is_err());

        let writes: Vec<_> = scripted
            .calls()
            .into_iter()
            .filter(|c| c.function == "ioctl_pio_outb")
            .map(|c| c.input)
            .collect();
        assert_eq!(
            writes,
            [
                vec![0x0295, 0x73],
                vec![0x0295, 0x4E],
                vec![0x0295, 0x4E],
                vec![0x0295, 0x4E],
                vec![0x0296, 0x01],
            ]
        );
    }

    #[test]
    fn only_banks_0_to_0x0f_and_the_high_byte_select_are_written() {
        let (scripted, module) = scripted();
        let mut chip = PawnSuperIo::new(module);
        chip.select_slot(SuperIoSlot::Primary).unwrap();
        for refused in [0x10, 0x40, 0x81, 0xFF] {
            chip.hwm_set_index(0x0290, 0x4E).unwrap();
            let error = chip.hwm_write_bank(0x0290, refused).unwrap_err();
            assert!(error.to_string().contains(&format!("bank {refused:#x}")), "{error}");
        }
        for accepted in [0x00, 0x0F, 0x80] {
            chip.hwm_set_index(0x0290, 0x4E).unwrap();
            chip.hwm_write_bank(0x0290, accepted).unwrap();
        }
        let data_port_writes: Vec<_> = scripted
            .calls()
            .into_iter()
            .filter(|c| c.function == "ioctl_pio_outb" && c.input[0] == 0x0296)
            .map(|c| c.input[1])
            .collect();
        assert_eq!(data_port_writes, [0x00, 0x0F, 0x80], "refused banks reach no port");
    }

    #[test]
    fn hwm_reads_use_the_address_and_data_ports() {
        let (scripted, module) = scripted();
        scripted.reply_ok(&[0x73]).reply_ok(&[0x2B]);
        let mut chip = PawnSuperIo::new(module);
        assert_eq!(chip.hwm_read_index(0x0290), Ok(0x73));
        assert_eq!(chip.hwm_read_data(0x0290), Ok(0x2B));
        assert_eq!(
            scripted.calls(),
            [call("ioctl_pio_inb", &[0x0295], 1), call("ioctl_pio_inb", &[0x0296], 1)]
        );
    }

    #[test]
    fn smbus_reads_use_byte_data_and_word_data_protocols_in_the_fixed_signature() {
        let (scripted, module) = scripted();
        scripted.reply_ok(&[0x0C]).reply_ok(&[0xC001]);
        let mut bus = PawnSmbus::new(module);
        assert_eq!(bus.read_byte_data(0x50, 2), Ok(0x0C));
        assert_eq!(bus.read_word_data(0x18, 5), Ok(0xC001));
        assert_eq!(
            scripted.calls(),
            [
                call("ioctl_smbus_xfer", &[0x50, 1, 2, 2, 0, 0, 0, 0, 0], 5),
                call("ioctl_smbus_xfer", &[0x18, 1, 5, 3, 0, 0, 0, 0, 0], 5),
            ],
            "nine input cells and five output cells, as the module's signature requires"
        );
    }

    #[test]
    fn a_read_value_wider_than_its_protocol_is_rejected() {
        let (scripted, module) = scripted();
        scripted.reply_ok(&[0x100]).reply_ok(&[0x1_0000]);
        let mut bus = PawnSmbus::new(module);
        let byte = bus.read_byte_data(0x50, 0).unwrap_err();
        assert_eq!(
            byte.to_string(),
            "(ioctl_smbus_xfer byte read): expected a value that fits one byte | received: 0x100"
        );
        let word = bus.read_word_data(0x18, 5).unwrap_err();
        assert_eq!(
            word.to_string(),
            "(ioctl_smbus_xfer word read): expected a value that fits one word | received: 0x10000"
        );
    }

    #[cfg(feature = "spd-paging")]
    #[test]
    fn the_only_smbus_write_targets_a_page_select_address() {
        let (scripted, module) = scripted();
        let mut bus = PawnSmbus::new(module);
        bus.select_spd_page(0).unwrap();
        bus.select_spd_page(1).unwrap();
        assert_eq!(
            scripted.calls(),
            [
                call("ioctl_smbus_xfer", &[0x36, 0, 0, 1, 0, 0, 0, 0, 0], 5),
                call("ioctl_smbus_xfer", &[0x37, 0, 0, 1, 0, 0, 0, 0, 0], 5),
            ]
        );
        for page in [2, 7, 0x36, 0xFF] {
            assert!(bus.select_spd_page(page).is_err(), "page {page} must be refused");
        }
        assert_eq!(scripted.calls().len(), 2, "refused pages must not reach the driver");
    }

    #[cfg(feature = "spd-paging")]
    #[test]
    #[should_panic(expected = "(smbus write): expected target 0x36 or 0x37 | received: 0x50")]
    fn a_write_to_a_device_address_trips_the_assertion_before_the_driver() {
        let (scripted, module) = scripted();
        let bus = PawnSmbus::new(module);
        // Only reachable from inside this module: the trait has no such method.
        let _ = bus.write_page_select(0x50);
        assert!(scripted.calls().is_empty());
    }

    /// The driver calls of one fan-register write at base `0x0A20`: bank
    /// select, index, data, then back to bank 0.
    #[cfg(feature = "fan-control")]
    fn fan_write_calls(bank: u8, index: u8, value: u8) -> Vec<RecordedCall> {
        vec![
            call("ioctl_pio_outb", &[0x0A25, 0x4E], 0),
            call("ioctl_pio_outb", &[0x0A26, u64::from(bank)], 0),
            call("ioctl_pio_outb", &[0x0A25, u64::from(index)], 0),
            call("ioctl_pio_outb", &[0x0A26, u64::from(value)], 0),
            call("ioctl_pio_outb", &[0x0A25, 0x4E], 0),
            call("ioctl_pio_outb", &[0x0A26, 0], 0),
        ]
    }

    #[cfg(feature = "fan-control")]
    #[test]
    fn a_fan_register_write_selects_the_bank_writes_the_data_and_leaves_bank_zero() {
        let (scripted, module) = scripted();
        let mut io = PawnSuperIo::new(module);
        io.hwm_write_fan_register(0x0A20, 0x0109, 0x80).unwrap();
        io.hwm_write_fan_register(0x0A20, 0x0902, 0x00).unwrap();
        let mut expected = fan_write_calls(0x01, 0x09, 0x80);
        expected.extend(fan_write_calls(0x09, 0x02, 0x00));
        assert_eq!(scripted.calls(), expected);
    }

    #[cfg(feature = "fan-control")]
    #[test]
    fn a_fan_register_write_to_any_other_register_never_reaches_the_driver() {
        let (scripted, module) = scripted();
        let mut io = PawnSuperIo::new(module);
        // The output type, a temperature source, the duty read-back, the
        // bank-select register itself, and a register on a bank only a later
        // chip has.
        for address in [0x0004, 0x0100, 0x0001, 0x004E, 0x0621, 0x0A09, 0x0B09, 0x0103, 0xFFFF] {
            let error = io.hwm_write_fan_register(0x0A20, address, 0xFF).unwrap_err();
            assert_eq!(
                error.to_string(),
                format!(
                    "(hwm_write_fan_register): expected a fan-mode or manual-duty register of the NCT6776F or NCT6779D | received: address {address:#06x}"
                )
            );
        }
        assert!(scripted.calls().is_empty(), "refused writes must not reach the driver");
    }

    #[cfg(feature = "fan-control")]
    #[test]
    fn a_failed_fan_register_write_still_returns_to_bank_zero_and_reports_the_error() {
        let (scripted, module) = scripted();
        // Bank select and bank number succeed; the index write fails.
        scripted
            .reply_ok(&[])
            .reply_ok(&[])
            .reply_err(DriverError::Timeout { function: "ioctl_pio_outb".into() });
        let mut io = PawnSuperIo::new(module);
        let error = io.hwm_write_fan_register(0x0A20, 0x0109, 0x80).unwrap_err();
        assert!(error.is_bus_fault(), "{error}");
        let calls = scripted.calls();
        assert_eq!(calls.len(), 5, "the data write is skipped, the bank reset is attempted");
        assert_eq!(calls[3], call("ioctl_pio_outb", &[0x0A25, 0x4E], 0));
        assert_eq!(calls[4], call("ioctl_pio_outb", &[0x0A26, 0], 0));
    }

    #[test]
    fn a_missing_smbus_device_is_a_no_device_error() {
        let (scripted, module) = scripted();
        scripted.reply_err(DriverError::NoSuchDevice { function: "ioctl_smbus_xfer".into() });
        let mut bus = PawnSmbus::new(module);
        assert!(bus.read_byte_data(0x57, 0).unwrap_err().is_no_device());
    }

    #[test]
    fn a_pch_read_is_one_call_with_no_input_and_names_the_route() {
        let (scripted, module) = scripted();
        scripted.reply_ok(&[0x00C8, 1]).reply_ok(&[0x8000_0231, 2]);
        let mut sensor = PawnPchThermal::new(module);
        assert_eq!(sensor.read_raw(), Ok(PchReading { raw: 0x00C8, route: PchRoute::PciBar0 }));
        assert_eq!(
            sensor.read_raw(),
            Ok(PchReading { raw: 0x8000_0231, route: PchRoute::FixedPwrm })
        );
        // The module has no write function; the only call is the raw read.
        assert_eq!(
            scripted.calls(),
            [call("ioctl_read_raw", &[], 2), call("ioctl_read_raw", &[], 2)]
        );
    }

    #[test]
    fn a_pch_read_with_an_unknown_route_or_a_wide_register_is_rejected() {
        let (scripted, module) = scripted();
        scripted.reply_ok(&[0x00C8, 3]).reply_ok(&[0x1_0000_0000, 2]);
        let mut sensor = PawnPchThermal::new(module);
        assert_eq!(
            sensor.read_raw().unwrap_err().to_string(),
            "(ioctl_read_raw): expected route 1 (PCI thermal BAR0) or 2 (fixed PWRM window) | \
             received: route 3"
        );
        assert_eq!(
            sensor.read_raw().unwrap_err().to_string(),
            "(ioctl_read_raw): expected a register value that fits 32 bits | received: 0x100000000"
        );
    }

    #[test]
    fn a_pch_read_that_the_module_declines_keeps_the_driver_error() {
        let (scripted, module) = scripted();
        scripted.reply_err(DriverError::NotReady { function: "ioctl_read_raw".into() });
        let mut sensor = PawnPchThermal::new(module);
        assert!(matches!(sensor.read_raw(), Err(BusError::Driver(DriverError::NotReady { .. }))));
    }
}
