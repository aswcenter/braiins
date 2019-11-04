// Copyright (C) 2019  Braiins Systems s.r.o.
//
// This file is part of Braiins Open-Source Initiative (BOSI).
//
// BOSI is free software: you can redistribute it and/or modify
// it under the terms of the GNU Common Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Common Public License for more details.
//
// You should have received a copy of the GNU Common Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.
//
// Please, keep in mind that we may also license BOSI or any part thereof
// under a proprietary license. For more information on the terms and conditions
// of such proprietary license or if you have any other questions, please
// contact us at opensource@braiins.com.

mod bm1387;
pub mod command;
pub mod config;
pub mod error;
pub mod fan;
pub mod gpio;
pub mod i2c;
pub mod io;
pub mod monitor;
pub mod null_work;
pub mod power;
pub mod registry;
pub mod sensor;
pub mod utils;

#[cfg(test)]
pub mod test;

use ii_logging::macros::*;

use bosminer::clap;
use bosminer::hal;
use bosminer::node;
use bosminer::runtime_config;
use bosminer::shutdown;
use bosminer::stats;
use bosminer::work;
use bosminer_macros::WorkSolverNode;

use std::fmt;
use std::sync::Arc;
use std::time::{self, Duration, Instant};

use lazy_static::lazy_static;

use error::ErrorKind;
use failure::ResultExt;

use core::future::Future;
use futures::channel::mpsc;
use futures::future::select;
use futures::future::FutureExt;
use futures::lock::Mutex;
use futures::stream::StreamExt;
use ii_async_compat::futures;

use bm1387::ChipAddress;
use command::Interface;

use packed_struct::PackedStruct;

use embedded_hal::digital::v2::InputPin;
use embedded_hal::digital::v2::OutputPin;

use ii_fpga_io_am1_s9::common::ctrl_reg::MIDSTATE_CNT_A;

use ii_bitcoin::MeetsTarget;

use ii_async_compat::tokio;
use tokio::sync::watch;
use tokio::timer::delay_for;

/// Timing constants
const INACTIVATE_FROM_CHAIN_DELAY_MS: u64 = 100;
/// Base delay quantum during hashboard initialization
const INIT_DELAY_MS: u64 = 1000;

/// Maximum number of chips is limitted by the fact that there is only 8-bit address field and
/// addresses to the chips need to be assigned with step of 4 (e.g. 0, 4, 8, etc.)
const MAX_CHIPS_ON_CHAIN: usize = 64;

/// Oscillator speed for all chips on S9 hash boards
const CHIP_OSC_CLK_HZ: usize = 25_000_000;

/// Exact value of the initial baud rate after reset of the hashing chips.
const INIT_CHIP_BAUD_RATE: usize = 115740;
/// Exact desired target baud rate when hashing at full speed (matches the divisor, too)
const TARGET_CHIP_BAUD_RATE: usize = 1562500;

/// Base clock speed of the IP core running in the FPGA
const FPGA_IPCORE_F_CLK_SPEED_HZ: usize = 50_000_000;
/// Divisor of the base clock. The resulting clock is connected to UART
const FPGA_IPCORE_F_CLK_BASE_BAUD_DIV: usize = 16;

/// Default initial voltage
const OPEN_CORE_VOLTAGE: power::Voltage = power::Voltage::from_volts(9.4);

/// Address of chip with connected temp sensor
const TEMP_CHIP: ChipAddress = ChipAddress::One(61);

lazy_static! {
    /// What is our target?
    static ref ASIC_TARGET: ii_bitcoin::Target =
        ii_bitcoin::Target::from_pool_difficulty(config::ASIC_DIFFICULTY);
}

/// `MidstateCount` represents the number of midstates S9 FPGA sends to chips.
/// This information needs to be accessible to everyone that processes `work_id`.
///
/// `MidstateCount` provides methods to encode number of midstates in various ways:
///  * bitmask to mask out parts of `solution_id`
///  * base-2 logarithm of number of midstates
///  * FPGA configuration value (which is base-2 logarithm, but as an enum)
///
/// `MidstateCount` is always valid - creation of `MidstateCount` object that isn't
/// supported by hardware shouldn't be possible.
#[derive(Debug, Clone, Copy)]
pub struct MidstateCount {
    /// internal representation is base-2 logarithm of number of midstates
    log2: usize,
}

impl MidstateCount {
    /// Construct Self, panic if number of midstates is not valid for this hw
    fn new(count: usize) -> Self {
        match count {
            1 => Self { log2: 0 },
            2 => Self { log2: 1 },
            4 => Self { log2: 2 },
            _ => panic!("Unsupported S9 Midstate count {}", count),
        }
    }

    /// Return midstate count encoded for FPGA
    fn to_reg(&self) -> MIDSTATE_CNT_A {
        match self.log2 {
            0 => MIDSTATE_CNT_A::ONE,
            1 => MIDSTATE_CNT_A::TWO,
            2 => MIDSTATE_CNT_A::FOUR,
            _ => panic!("invalid internal midstate count logarithm"),
        }
    }

    /// Return midstate count
    #[inline]
    fn to_count(&self) -> usize {
        1 << self.log2
    }

    /// Return log2 of midstate count
    #[inline]
    fn to_bits(&self) -> usize {
        self.log2
    }

    /// Return midstate count mask (to get midstate_idx bits from `work_id`)
    #[inline]
    fn to_mask(&self) -> usize {
        (1 << self.log2) - 1
    }
}

/// Sender of `Halt` condition
#[derive(Clone)]
struct HaltSender {
    inner: Arc<Mutex<watch::Sender<bool>>>,
}

impl HaltSender {
    /// Broadcast `Halt` condition
    async fn do_stop(&self) {
        self.inner
            .lock()
            .await
            .broadcast(true)
            .expect("restart broadcasting failed");
        // TODO: this is a hack, we should collect "halt status" from all receivers and return
        // once we've collected them all.
        delay_for(Duration::from_secs(2)).await;
    }
}

/// Receiver of `Halt` condition
#[derive(Clone)]
struct HaltReceiver {
    inner: watch::Receiver<bool>,
}

impl HaltReceiver {
    /// Wait for `Halt` to be broadcasted
    async fn wait_for_halt(&mut self) {
        loop {
            match self.inner.next().await {
                None => {
                    error!("Owner dropped HaltSender, no one to stop us now! Shutting down task.");
                    break;
                }
                Some(halt) => {
                    if halt {
                        break;
                    }
                }
            }
        }
    }

    /// Spawn a new task that is dropped when `Halt` is received
    fn spawn<F>(&self, f: F)
    where
        F: Future<Output = ()> + 'static + Send,
    {
        let mut receiver = self.clone();
        tokio::spawn(async move {
            select(f.boxed(), receiver.wait_for_halt().boxed()).await;
        });
    }
}

fn make_halt_pair() -> (HaltSender, HaltReceiver) {
    let (tx, rx) = watch::channel(false);
    (
        HaltSender {
            inner: Arc::new(Mutex::new(tx)),
        },
        HaltReceiver { inner: rx },
    )
}

/// Core address space size (it should be 114, but the addresses are non-consecutive)
const CORE_ADR_SPACE_SIZE: usize = 128;

/// Per-core counters for valid nonces/errors
#[derive(Clone, Copy)]
pub struct CoreCounter {
    valid: usize,
    errors: usize,
}

impl CoreCounter {
    pub fn reset(&mut self) {
        self.valid = 0;
        self.errors = 0;
    }

    pub fn new() -> Self {
        Self {
            valid: 0,
            errors: 0,
        }
    }
}

#[derive(Clone, Copy)]
pub struct ChipCounter {
    core: [CoreCounter; CORE_ADR_SPACE_SIZE],
    valid: usize,
    errors: usize,
    started: Instant,
}

impl ChipCounter {
    pub fn reset(&mut self) {
        self.valid = 0;
        self.errors = 0;
        for core in self.core.iter_mut() {
            core.reset();
        }
        self.started = Instant::now();
    }

    pub fn core_bitmask(&self) -> u128 {
        let mut mask: u128 = 0;
        for (id, core) in self.core.iter().enumerate() {
            if core.valid > 0 {
                mask |= 1u128 << id;
            }
        }
        mask
    }

    pub fn dead_cores(&self) -> usize {
        let mask = self.core_bitmask();
        let mask = ((mask << 1) | (mask >> 1) | mask) & 0x1ffffffffffffff01ffffffffffffff;
        114 - mask.count_ones() as usize
    }

    pub fn new() -> Self {
        Self {
            valid: 0,
            errors: 0,
            core: [CoreCounter::new(); CORE_ADR_SPACE_SIZE],
            started: Instant::now(),
        }
    }
}

#[derive(Clone)]
pub struct HashChainCounter {
    chip: Vec<ChipCounter>,
    valid: usize,
    errors: usize,
    started: Instant,
}

impl HashChainCounter {
    pub fn reset(&mut self) {
        self.valid = 0;
        self.errors = 0;
        for chip in self.chip.iter_mut() {
            chip.reset();
        }
        self.started = Instant::now();
    }

    pub fn add_valid(&mut self, addr: bm1387::CoreAddress) {
        if addr.chip >= self.chip.len() {
            // nonce from non-existent chip
            // TODO: what to do?
            return;
        }
        self.valid += 1;
        self.chip[addr.chip].valid += 1;
        self.chip[addr.chip].core[addr.core].valid += 1;
    }

    pub fn add_error(&mut self, addr: bm1387::CoreAddress) {
        if addr.chip >= self.chip.len() {
            // nonce from non-existent chip
            // TODO: what to do?
            return;
        }
        self.errors += 1;
        self.chip[addr.chip].errors += 1;
        self.chip[addr.chip].core[addr.core].errors += 1;
    }

    pub fn new(chip_count: usize) -> Self {
        Self {
            valid: 0,
            errors: 0,
            started: Instant::now(),
            chip: vec![ChipCounter::new(); chip_count],
        }
    }
}

/// Stateful wrapper around voltage control
struct PowerStation {
    voltage_ctrl: power::Control<power::SharedBackend<power::I2cBackend>>,
    current_voltage: Option<power::Voltage>,
}

impl PowerStation {
    pub fn new(
        voltage_ctrl_backend: power::SharedBackend<power::I2cBackend>,
        hashboard_idx: usize,
    ) -> Self {
        Self {
            voltage_ctrl: power::Control::new(voltage_ctrl_backend, hashboard_idx),
            current_voltage: None,
        }
    }

    /// Set voltage (and remember what was set)
    pub async fn set_voltage(&mut self, voltage: power::Voltage) -> error::Result<()> {
        // Check if we have to change the voltage
        if self.current_voltage != Some(voltage) {
            self.voltage_ctrl.set_voltage(voltage).await?;
            self.current_voltage = Some(voltage);
        }
        Ok(())
    }

    /// Initialize voltage controller
    async fn init(&mut self, mut halt_rx: HaltReceiver) -> error::Result<()> {
        self.voltage_ctrl.reset().await?;
        info!("Voltage controller reset");
        self.voltage_ctrl.jump_from_loader_to_app().await?;
        info!("Voltage controller application started");
        let version = self.voltage_ctrl.get_version()?;
        info!("Voltage controller firmware version {:#04x}", version);
        // TODO accept multiple
        if version != power::EXPECTED_VOLTAGE_CTRL_VERSION {
            Err(ErrorKind::UnexpectedVersion(
                "voltage controller firmware".to_string(),
                version.to_string(),
                power::EXPECTED_VOLTAGE_CTRL_VERSION.to_string(),
            ))?
        }
        self.set_voltage(OPEN_CORE_VOLTAGE).await?;
        self.voltage_ctrl.enable_voltage()?;

        // Voltage controller successfully initialized at this point, we should start sending
        // heart beats to it. Otherwise, it would shut down in about 10 seconds.
        info!("Starting voltage controller heart beat task");
        let stop_heart_beat = self.voltage_ctrl.start_heart_beat_task();

        // On `Halt` we have to stop the thread sending heart beat
        let mut voltage_ctrl = self.voltage_ctrl.clone();
        tokio::spawn(async move {
            // Wait for halt
            halt_rx.wait_for_halt().await;
            // Turn off voltage on chain
            voltage_ctrl
                .disable_voltage()
                .expect("failed disabling voltage");
            // Then stop and join the thread
            // TODO: this may take up to 1 second, figure out a way to stop it immediately
            stop_heart_beat.stop();
        });

        Ok(())
    }
}

pub struct HashChainPll {
    chip_freq: Vec<Option<usize>>,
    max_freq: Option<usize>,
}

/// Hash Chain Controller provides abstraction of the FPGA interface for operating hashing boards.
/// It is the user-space driver for the IP Core
///
/// Main responsibilities:
/// - memory mapping of the FPGA control interface
/// - mining work submission and solution processing
pub struct HashChain {
    /// Number of chips that have been detected
    chip_count: usize,
    /// Eliminates the need to query the IP core about the current number of configured midstates
    midstate_count: MidstateCount,
    /// ASIC difficulty
    asic_difficulty: usize,
    /// current PLL chip frequency
    pll: Mutex<HashChainPll>,
    /// Voltage controller on this hashboard
    power_station: Mutex<PowerStation>,
    /// Plug pin that indicates the hashboard is present
    #[allow(dead_code)]
    plug_pin: gpio::PinIn,
    /// Pin for resetting the hashboard
    rst_pin: gpio::PinOut,
    /// When the heartbeat was last sent
    #[allow(dead_code)]
    last_heartbeat_sent: Option<time::Instant>,
    hashboard_idx: usize,
    pub command_context: command::Context,
    pub common_io: io::Common,
    work_rx_io: Mutex<Option<io::WorkRx>>,
    work_tx_io: Mutex<Option<io::WorkTx>>,
    monitor_tx: mpsc::UnboundedSender<monitor::Message>,
    /// Do not send open-core work if this is true (some tests that test chip initialization may
    /// want to do this).
    disable_init_work: bool,
}

impl HashChain {
    /// Creates a new hashboard controller with memory mapped FPGA IP core
    ///
    /// * `gpio_mgr` - gpio manager used for producing pins required for hashboard control
    /// * `voltage_ctrl_backend` - communication backend for the voltage controller
    /// * `hashboard_idx` - index of this hashboard determines which FPGA IP core is to be mapped
    /// * `midstate_count` - see Self
    /// * `asic_difficulty` - to what difficulty set the hardware target filter
    pub fn new(
        gpio_mgr: &gpio::ControlPinManager,
        voltage_ctrl_backend: power::SharedBackend<power::I2cBackend>,
        hashboard_idx: usize,
        midstate_count: MidstateCount,
        asic_difficulty: usize,
        monitor_tx: mpsc::UnboundedSender<monitor::Message>,
    ) -> error::Result<Self> {
        // Hashboard creation is aborted if the pin is not present
        let plug_pin = gpio_mgr
            .get_pin_in(gpio::PinInName::Plug(hashboard_idx))
            .context(ErrorKind::Hashboard(
                hashboard_idx,
                "failed to initialize plug pin".to_string(),
            ))?;
        // also detect that the board is present
        if plug_pin.is_low()? {
            Err(ErrorKind::Hashboard(
                hashboard_idx,
                "not present".to_string(),
            ))?
        }

        // Instantiate the reset pin
        let rst_pin = gpio_mgr
            .get_pin_out(gpio::PinOutName::Rst(hashboard_idx))
            .context(ErrorKind::Hashboard(
                hashboard_idx,
                "failed to initialize reset pin".to_string(),
            ))?;

        let core = io::Core::new(hashboard_idx, midstate_count)?;
        // Unfortunately, we have to do IP core re-init here (but it should be OK, it's synchronous)
        let (common_io, command_io, work_rx_io, work_tx_io) = core.init_and_split()?;

        Ok(Self {
            chip_count: 0,
            midstate_count,
            asic_difficulty,
            power_station: Mutex::new(PowerStation::new(voltage_ctrl_backend, hashboard_idx)),
            plug_pin,
            rst_pin,
            hashboard_idx,
            last_heartbeat_sent: None,
            common_io,
            command_context: command::Context::new(command_io),
            work_rx_io: Mutex::new(Some(work_rx_io)),
            work_tx_io: Mutex::new(Some(work_tx_io)),
            monitor_tx,
            disable_init_work: false,
            pll: Mutex::new(HashChainPll {
                chip_freq: [None; MAX_CHIPS_ON_CHAIN].to_vec(),
                max_freq: None,
            }),
        })
    }

    async fn take_work_rx_io(&self) -> io::WorkRx {
        self.work_rx_io
            .lock()
            .await
            .take()
            .expect("work-rx io missing")
    }

    async fn take_work_tx_io(&self) -> io::WorkTx {
        self.work_tx_io
            .lock()
            .await
            .take()
            .expect("work-tx io missing")
    }

    /// Calculate work_time for this instance of HChain
    ///
    /// Returns number of ticks (suitable to be written to `WORK_TIME` register)
    #[inline]
    fn calculate_work_time(&self, max_pll_frequency: usize) -> u32 {
        secs_to_fpga_ticks(calculate_work_delay_for_pll(
            self.midstate_count.to_count(),
            max_pll_frequency,
        ))
    }

    /// Set work time depending on current PLL frequency
    ///
    /// This method sets work time so it's fast enough at least for `new_freq`.
    /// It internally tracks what work_time is set and when it's too slow, it increases it.
    async fn set_work_time(&self, new_freq: usize) {
        let mut pll = self.pll.lock().await;
        let current_max_freq = pll.max_freq.unwrap_or(0);

        if new_freq > current_max_freq {
            let new_work_time = self.calculate_work_time(new_freq);
            info!("Using work time: {} for freq {}", new_work_time, new_freq);
            self.common_io.set_ip_core_work_time(new_work_time);
            pll.max_freq = Some(new_freq);
        }
    }

    /// Helper method that initializes the FPGA IP core
    async fn ip_core_init(&mut self) -> error::Result<()> {
        // Configure IP core
        self.set_ip_core_baud_rate(INIT_CHIP_BAUD_RATE)?;
        self.common_io.set_midstate_count();

        Ok(())
    }

    /// Puts the board into reset mode and disables the associated IP core
    fn enter_reset(&mut self) -> error::Result<()> {
        self.common_io.disable_ip_core();
        // perform reset of the hashboard
        self.rst_pin.set_low()?;
        Ok(())
    }

    /// Leaves reset mode
    fn exit_reset(&mut self) -> error::Result<()> {
        self.rst_pin.set_high()?;
        self.common_io.enable_ip_core();
        Ok(())
    }

    /// Configures difficulty globally on all chips within the hashchain
    async fn set_asic_diff(&mut self, difficulty: usize) -> error::Result<()> {
        let tm_reg = bm1387::TicketMaskReg::new(difficulty as u32)?;
        trace!(
            "Setting ticket mask register for difficulty {}, value {:#010x?}",
            difficulty,
            tm_reg
        );
        self.command_context
            .write_register_readback(ChipAddress::All, &tm_reg)
            .await?;
        Ok(())
    }

    /// Initializes the complete hashboard including enumerating all chips
    async fn init(
        &mut self,
        halt_rx: HaltReceiver,
        initial_pll_frequency: Vec<usize>,
        initial_voltage: power::Voltage,
    ) -> error::Result<Arc<Mutex<registry::WorkRegistry>>> {
        info!("Registering ourselves with monitor");
        self.monitor_tx
            .unbounded_send(monitor::Message::On)
            .expect("send failed");
        info!("Initializing hash chain {}", self.hashboard_idx);
        self.ip_core_init().await?;
        info!("Hashboard IP core initialized");
        self.power_station.lock().await.init(halt_rx).await?;
        info!("Resetting hash board");
        self.enter_reset()?;
        // disable voltage
        self.power_station
            .lock()
            .await
            .voltage_ctrl
            .disable_voltage()?;
        delay_for(Duration::from_millis(INIT_DELAY_MS)).await;
        self.power_station
            .lock()
            .await
            .voltage_ctrl
            .enable_voltage()?;
        delay_for(Duration::from_millis(2 * INIT_DELAY_MS)).await;

        // TODO consider including a delay
        self.exit_reset()?;
        delay_for(Duration::from_millis(INIT_DELAY_MS)).await;
        //        let voltage = self.voltage_ctrl.get_voltage()?;
        //        if voltage != 0 {
        //            return Err(io::Error::new(
        //                io::ErrorKind::Other, format!("Detected voltage {}", voltage)));
        //        }
        info!("Starting chip enumeration");
        self.enumerate_chips().await?;
        info!("Discovered {} chips", self.chip_count);
        self.command_context.set_chip_count(self.chip_count).await;

        // set PLL
        self.set_pll_table(initial_pll_frequency).await?;

        // configure the hashing chain to operate at desired baud rate. Note that gate block is
        // enabled to allow continuous start of chips in the chain
        self.configure_hash_chain(TARGET_CHIP_BAUD_RATE, false, true)
            .await?;
        self.set_ip_core_baud_rate(TARGET_CHIP_BAUD_RATE)?;

        self.set_asic_diff(self.asic_difficulty).await?;

        // Build shared work registry
        // TX fifo determines the size of work registry
        let work_registry = Arc::new(Mutex::new(registry::WorkRegistry::new(
            self.work_tx_io
                .lock()
                .await
                .as_ref()
                .expect("work-tx io missing")
                .work_id_count(),
        )));

        // send opencore work (at high voltage) unless someone disabled it
        if !self.disable_init_work {
            self.send_init_work(work_registry.clone()).await;
        }

        // lower voltage to working level
        self.power_station
            .lock()
            .await
            .set_voltage(initial_voltage)
            .await
            .expect("lowering voltage failed");

        // return work registry we created
        Ok(work_registry)
    }

    /// Detects the number of chips on the hashing chain and assigns an address to each chip
    async fn enumerate_chips(&mut self) -> error::Result<()> {
        // Enumerate all chips (broadcast read address register request)
        let responses = self
            .command_context
            .read_register::<bm1387::GetAddressReg>(ChipAddress::All)
            .await?;

        // Check if are responses meaningful
        for (address, addr_reg) in responses.iter().enumerate() {
            if addr_reg.chip_rev != bm1387::ChipRev::Bm1387 {
                Err(ErrorKind::Hashchip(format!(
                    "unexpected revision of chip {} (expected: {:?} received: {:?})",
                    address,
                    addr_reg.chip_rev,
                    bm1387::ChipRev::Bm1387
                )))?
            }
            self.chip_count += 1;
        }

        if self.chip_count >= MAX_CHIPS_ON_CHAIN {
            Err(ErrorKind::Hashchip(format!(
                "detected {} chips, expected less than 256 chips on 1 chain. Possibly a hardware issue?",
                self.chip_count
            )))?
        }
        if self.chip_count == 0 {
            Err(ErrorKind::Hashchip(
                "no chips detected on the current chain".to_string(),
            ))?
        }
        // Set all chips to be offline before address assignment. This is important so that each
        // chip after initially accepting the address will pass on further addresses down the chain
        let inactivate_from_chain_cmd = bm1387::InactivateFromChainCmd::new().pack();
        // make sure all chips receive inactivation request
        for _ in 0..3 {
            self.command_context
                .send_raw_command(inactivate_from_chain_cmd.to_vec(), false)
                .await;
            delay_for(Duration::from_millis(INACTIVATE_FROM_CHAIN_DELAY_MS)).await;
        }

        // Assign address to each chip
        for i in 0..self.chip_count {
            let cmd = bm1387::SetChipAddressCmd::new(ChipAddress::One(i));
            self.command_context
                .send_raw_command(cmd.pack().to_vec(), false)
                .await;
        }

        Ok(())
    }

    /// Loads PLL register with a starting value
    async fn set_pll(&self, freq: usize, chip_id: usize) -> error::Result<()> {
        assert!(chip_id < self.chip_count);

        // check we are setting a different frequency
        let mut pll = self.pll.lock().await;
        if pll.chip_freq[chip_id] == Some(freq) {
            // frequency is the same, do nothing
            return Ok(());
        }

        // convert frequency to PLL setting register
        let pll_reg = bm1387::PllReg::try_pll_from_freq(CHIP_OSC_CLK_HZ, freq)?;

        info!(
            "setting frequency {} MHz on chip {} (real {})",
            freq / 1_000_000,
            chip_id,
            pll_reg.calc(CHIP_OSC_CLK_HZ)
        );
        // NOTE: when PLL register is read back, it is or-ed with 0x8000_0000, not sure why
        self.command_context
            .write_register(ChipAddress::One(chip_id), &pll_reg)
            .await?;

        // Remember the frequency we set
        pll.chip_freq[chip_id] = Some(freq);
        drop(pll);

        // Update worktime if necessary
        self.set_work_time(freq).await;

        Ok(())
    }

    /// Load PLL register of all chips
    async fn set_pll_table(&self, freq_table: Vec<usize>) -> error::Result<()> {
        // TODO: find a better way - how to communicate with frequency setter how many chips we have?
        assert!(freq_table.len() >= self.chip_count);

        for i in 0..self.chip_count {
            self.set_pll(freq_table[i], i).await?;
        }
        Ok(())
    }

    /// Configure all chips in the hash chain
    ///
    /// This method programs the MiscCtrl register of each chip in the hash chain.
    ///
    /// * `baud_rate` - desired communication speed
    /// * `not_set_baud` - the baud clock divisor is calculated, however, each chip will ignore
    /// its value. This is used typically when gate_block is enabled.
    /// * `gate_block` - allows gradual startup of the chips in the chain as they keep receiving
    /// special 'null' job. See bm1387::MiscCtrlReg::gate_block for details
    ///
    /// Returns actual baud rate that has been set on the chips or an error
    /// @todo Research the exact use case of 'not_set_baud' in conjunction with gate_block
    async fn configure_hash_chain(
        &mut self,
        baud_rate: usize,
        not_set_baud: bool,
        gate_block: bool,
    ) -> error::Result<usize> {
        let (baud_clock_div, actual_baud_rate) = calc_baud_clock_div(
            baud_rate,
            CHIP_OSC_CLK_HZ,
            bm1387::CHIP_OSC_CLK_BASE_BAUD_DIV,
        )?;
        info!(
            "Setting Hash chain baud rate @ requested: {}, actual: {}, divisor {:#04x}",
            baud_rate, actual_baud_rate, baud_clock_div
        );
        // Each chip is always configured with inverted clock
        let ctl_reg =
            bm1387::MiscCtrlReg::new(not_set_baud, true, baud_clock_div, gate_block, true)?;
        // Do not read back the MiscCtrl register when setting baud rate: it will result
        // in serial speed mismatch and nothing being read.
        self.command_context
            .write_register(ChipAddress::All, &ctl_reg)
            .await?;
        Ok(actual_baud_rate)
    }

    /// This method only changes the communication speed of the FPGA IP core with the chips.
    ///
    /// Note: change baud rate of the FPGA is only desirable as a step after all chips in the
    /// chain have been reconfigured for a different speed, too.
    fn set_ip_core_baud_rate(&self, baud: usize) -> error::Result<()> {
        let (baud_clock_div, actual_baud_rate) = calc_baud_clock_div(
            baud,
            FPGA_IPCORE_F_CLK_SPEED_HZ,
            FPGA_IPCORE_F_CLK_BASE_BAUD_DIV,
        )?;
        info!(
            "Setting IP core baud rate @ requested: {}, actual: {}, divisor {:#04x}",
            baud, actual_baud_rate, baud_clock_div
        );

        self.common_io.set_baud_clock_div(baud_clock_div as u32);
        Ok(())
    }

    pub fn get_chip_count(&self) -> usize {
        self.chip_count
    }

    /// Initialize cores by sending open-core work with correct nbits to each core
    async fn send_init_work(&mut self, work_registry: Arc<Mutex<registry::WorkRegistry>>) {
        // Each core gets one work
        const NUM_WORK: usize = bm1387::NUM_CORES_ON_CHIP;
        trace!(
            "Sending out {} pieces of dummy work to initialize chips",
            NUM_WORK
        );
        let midstate_count = self.midstate_count.to_count();
        let mut work_tx_io = self.work_tx_io.lock().await;
        let tx_fifo = work_tx_io.as_mut().expect("tx fifo missing");
        for _ in 0..NUM_WORK {
            let work = &null_work::prepare_opencore(true, midstate_count);
            // store work to registry as "initial work" so that later we can properly ignore
            // solutions
            let work_id = work_registry.lock().await.store_work(work.clone(), true);
            tx_fifo.wait_for_room().await.expect("wait for tx room");
            tx_fifo.send_work(&work, work_id).expect("send work");
        }
    }

    /// This task picks up work from frontend (via generator), saves it to
    /// registry (to pair with `Assignment` later) and sends it out to hw.
    /// It makes sure that TX fifo is empty before requesting work from
    /// generator.
    /// It exits when generator returns `None`.
    async fn work_tx_task(
        work_registry: Arc<Mutex<registry::WorkRegistry>>,
        mut tx_fifo: io::WorkTx,
        mut work_generator: work::Generator,
    ) {
        loop {
            tx_fifo.wait_for_room().await.expect("wait for tx room");
            let work = work_generator.generate().await;
            match work {
                None => return,
                Some(work) => {
                    // assign `work_id` to `work`
                    let work_id = work_registry.lock().await.store_work(work.clone(), false);
                    // send work is synchronous
                    tx_fifo.send_work(&work, work_id).expect("send work");
                }
            }
        }
    }

    /// This task receives solutions from hardware, looks up `Assignment` in
    /// registry (under `work_id` got from FPGA), pairs them together and
    /// sends them back to frontend (via `solution_sender`).
    /// If solution is duplicated, it gets dropped (and errors stats incremented).
    /// It prints warnings when solution doesn't hit ASIC target.
    /// TODO: this task is not very platform dependent, maybe move it somewhere else?
    /// TODO: figure out when and how to stop this task
    async fn solution_rx_task(
        work_registry: Arc<Mutex<registry::WorkRegistry>>,
        mut rx_fifo: io::WorkRx,
        solution_sender: work::SolutionSender,
        counter: Arc<Mutex<HashChainCounter>>,
    ) {
        // solution receiving/filtering part
        loop {
            let (rx_fifo_out, solution) =
                rx_fifo.recv_solution().await.expect("recv solution failed");
            rx_fifo = rx_fifo_out;
            let work_id = solution.hardware_id;
            let mut work_registry = work_registry.lock().await;

            let work = work_registry.find_work(work_id as usize);
            match work {
                Some(work_item) => {
                    // ignore initial work
                    if work_item.initial_work {
                        continue;
                    }
                    let core_addr = bm1387::CoreAddress::new(solution.nonce);
                    let status = work_item.insert_solution(solution);

                    // work item detected a new unique solution, we will push it for further processing
                    if let Some(unique_solution) = status.unique_solution {
                        if !status.duplicate {
                            let hash = unique_solution.hash();
                            if !hash.meets(&ASIC_TARGET) {
                                warn!("Solution from hashchain not hitting ASIC target");
                                counter.lock().await.add_error(core_addr);
                            } else {
                                counter.lock().await.add_valid(core_addr);
                            }
                            solution_sender.send(unique_solution);
                        }
                    }
                    if status.duplicate {
                        counter.lock().await.add_error(core_addr);
                    }
                    if status.mismatched_nonce {
                        counter.lock().await.add_error(core_addr);
                    }
                }
                None => {
                    info!(
                        "No work present for solution, ID:{:#x} {:#010x?}",
                        work_id, solution
                    );
                }
            }
        }
    }

    /// Temperature monitor task
    async fn temperature_monitor_task(
        command_context: command::Context,
        monitor_tx: mpsc::UnboundedSender<monitor::Message>,
    ) {
        info!("Temperature monitor task started");
        // Wait some time before trying to initialize temperature controller
        // (Otherwise RX queue might be clogged with initial work and we will not get any replies)
        delay_for(Duration::from_secs(5)).await;
        let i2c_bus = bm1387::i2c::Bus::new_and_init(command_context, TEMP_CHIP)
            .await
            .expect("bus construction failed");
        let mut sensor = sensor::probe_i2c_sensors(i2c_bus)
            .await
            .expect("sensor probing failed")
            .expect("no sensors found");
        sensor.init().await.expect("failed to initialize sensor");
        loop {
            let temp = sensor
                .read_temperature()
                .await
                .expect("failed to read temperature");
            info!("Measured temperature: {:?}", temp);
            monitor_tx
                .unbounded_send(monitor::Message::Running(temp))
                .expect("send failed");
            // TODO: sync this delay with monitor task
            delay_for(Duration::from_secs(5)).await;
        }
    }

    /// Hashrate monitor task
    /// Fetch perodically information about hashrate
    async fn hashrate_monitor_task(command_context: command::Context) {
        info!("Hashrate monitor task started");
        loop {
            delay_for(Duration::from_secs(5)).await;

            let responses = command_context
                .read_register::<bm1387::HashrateReg>(ChipAddress::All)
                .await
                .expect("reading hashrate_reg failed");

            let mut sum = 0;
            for (chip_address, hashrate_reg) in responses.iter().enumerate() {
                trace!(
                    "chip {} hashrate {} GHash/s",
                    chip_address,
                    hashrate_reg.hashrate() as f64 / 1e9
                );
                sum += hashrate_reg.hashrate() as u128;
            }
            info!("Total chip hashrate {} GH/s", sum as f64 / 1e9);
        }
    }

    fn spawn_tx_task(
        self: Arc<Self>,
        work_registry: Arc<Mutex<registry::WorkRegistry>>,
        work_generator: work::Generator,
        shutdown: shutdown::Sender,
        halt_rx: HaltReceiver,
    ) {
        halt_rx.spawn(async move {
            let tx_fifo = self.take_work_tx_io().await;
            Self::work_tx_task(work_registry, tx_fifo, work_generator).await;
            shutdown.send("no more work from workhub");
        });
    }

    fn spawn_rx_task(
        self: Arc<Self>,
        work_registry: Arc<Mutex<registry::WorkRegistry>>,
        solution_sender: work::SolutionSender,
        halt_rx: HaltReceiver,
        counter: Arc<Mutex<HashChainCounter>>,
    ) {
        halt_rx.spawn(async move {
            let rx_fifo = self.take_work_rx_io().await;

            Self::solution_rx_task(work_registry, rx_fifo, solution_sender, counter).await;
        });
    }

    fn spawn_hashrate_monitor_task(command_context: command::Context, halt_rx: HaltReceiver) {
        halt_rx.spawn(async move {
            Self::hashrate_monitor_task(command_context).await;
        });
    }

    fn spawn_temperature_monitor_task(
        command_context: command::Context,
        halt_rx: HaltReceiver,
        monitor_tx: mpsc::UnboundedSender<monitor::Message>,
    ) {
        halt_rx.spawn(async move {
            Self::temperature_monitor_task(command_context, monitor_tx).await;
        });
    }

    async fn start(
        self: Arc<Self>,
        work_generator: work::Generator,
        work_solution: work::SolutionSender,
        shutdown: shutdown::Sender,
        halt_rx: HaltReceiver,
        counter: Arc<Mutex<HashChainCounter>>,
        work_registry: Arc<Mutex<registry::WorkRegistry>>,
    ) {
        let command_context = self.command_context.clone();

        Self::spawn_tx_task(
            self.clone(),
            work_registry.clone(),
            work_generator,
            shutdown.clone(),
            halt_rx.clone(),
        );
        Self::spawn_rx_task(
            self.clone(),
            work_registry.clone(),
            work_solution,
            halt_rx.clone(),
            counter,
        );
        Self::spawn_hashrate_monitor_task(command_context.clone(), halt_rx.clone());
        Self::spawn_temperature_monitor_task(
            command_context.clone(),
            halt_rx.clone(),
            self.monitor_tx.clone(),
        );
    }
}

impl fmt::Debug for HashChain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash Board {}", self.hashboard_idx)
    }
}

impl fmt::Display for HashChain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash Board {}", self.hashboard_idx)
    }
}

/// Mining parameters that can change run-time
#[derive(Clone)]
pub struct HashChainParams {
    pll_frequency: Vec<usize>,
    voltage: power::Voltage,
}

/// Hashchain and related runtime data
pub struct HashChainRuntime {
    halt_tx: HaltSender,
    #[allow(dead_code)]
    halt_rx: HaltReceiver,
    hash_chain: Arc<HashChain>,
    counter: Arc<Mutex<HashChainCounter>>,
}

#[derive(Default, Debug, WorkSolverNode)]
struct HashChainNode {
    #[member_work_solver_stats]
    work_solver_stats: stats::BasicWorkSolver,
    hashboard_idx: usize,
}

impl fmt::Display for HashChainNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Antminer S9 - Hashboard {}", self.hashboard_idx)
    }
}

/// Hashchain manager that can start and stop instances of hashchain
pub struct HashChainManager {
    // init
    gpio_mgr: gpio::ControlPinManager,
    voltage_ctrl_backend: power::SharedBackend<power::I2cBackend>,
    hashboard_idx: usize,
    midstate_count: MidstateCount,
    asic_difficulty: usize,
    // start (static)
    work_generator: work::Generator,
    work_solution: work::SolutionSender,
    shutdown: shutdown::Sender,
    // dynamic runtime - `is_some` only when miner is running
    runtime: Option<HashChainRuntime>,
    // configuration
    // monitor channel
    monitor_tx: mpsc::UnboundedSender<monitor::Message>,
    pub params: HashChainParams,
}

impl HashChainManager {
    /// Initialize and start mining on hashchain
    async fn start(&mut self) -> error::Result<()> {
        // check if we are running
        if self.runtime.is_some() {
            panic!("trying to start running chain");
        }

        // make us a hash chain
        let mut hash_chain = HashChain::new(
            &self.gpio_mgr,
            self.voltage_ctrl_backend.clone(),
            self.hashboard_idx,
            self.midstate_count,
            self.asic_difficulty,
            self.monitor_tx.clone(),
        )
        .expect("hashchain instantiation failed");

        // construct a way to signal hashchain halt
        let (halt_tx, halt_rx) = make_halt_pair();

        // make counter to track performance of chips
        let counter = Arc::new(Mutex::new(HashChainCounter::new()));

        // initialize it
        // halt is required to stop voltage heart-beat task
        let work_registry = hash_chain
            .init(
                halt_rx.clone(),
                self.params.pll_frequency.clone(),
                self.params.voltage.clone(),
            )
            .await
            .expect("hashchain initialization failed");

        // spawn worker tasks for hash chain and start mining
        let hash_chain = Arc::new(hash_chain);
        hash_chain
            .clone()
            .start(
                self.work_generator.clone(),
                self.work_solution.clone(),
                self.shutdown.clone(),
                halt_rx.clone(),
                counter.clone(),
                work_registry,
            )
            .await;

        // remember we are running
        self.runtime = Some(HashChainRuntime {
            halt_tx: halt_tx.clone(),
            halt_rx: halt_rx.clone(),
            hash_chain,
            counter,
        });

        Ok(())
    }

    /// Stop running hashchain
    async fn stop(&mut self) -> error::Result<()> {
        // check if we are running
        let runtime = match self.runtime.as_ref() {
            Some(runtime) => runtime,
            None => panic!("trying to stop non-running chain"),
        };
        // stop everything
        runtime.halt_tx.do_stop().await;
        // drop hashchain we keep around
        self.runtime = None;

        Ok(())
    }

    /// Return whether is hashchain running
    fn chain_is_running(&mut self) -> bool {
        self.runtime.is_some()
    }

    /// Set parameters of hashchain (both running and stopped)
    async fn set_params(&mut self, params: &HashChainParams) -> error::Result<()> {
        self.params = params.clone();

        if let Some(runtime) = self.runtime.as_ref() {
            // We are running, change parameters on a live hashchain instance as well
            runtime
                .hash_chain
                .power_station
                .lock()
                .await
                .set_voltage(params.voltage)
                .await?;
            runtime
                .hash_chain
                .set_pll_table(params.pll_frequency.clone())
                .await?;
        }
        self.params = params.clone();

        Ok(())
    }
}

/// Start miner
///
/// This will change once restarts are added
async fn start_miner(
    enabled_chains: Vec<usize>,
    work_solver_builder: work::SolverBuilder,
    shutdown: shutdown::Sender,
    midstate_count: usize,
    pll_frequency: usize,
    voltage: power::Voltage,
) {
    let config = monitor::Config {
        temp_config: Some(monitor::TempControlConfig {
            dangerous_temp: 110.0,
            hot_temp: 95.0,
        }),
        fan_config: Some(monitor::FanControlConfig {
            mode: monitor::FanControlMode::FixedSpeed(fan::Speed::FULL_SPEED),
            min_fans: 2,
        }),
    };
    let monitor = monitor::Monitor::new(config);
    let gpio_mgr = gpio::ControlPinManager::new();
    let voltage_ctrl_backend = power::I2cBackend::new(0);
    let voltage_ctrl_backend = power::SharedBackend::new(voltage_ctrl_backend);
    let mut hms = Vec::new();
    info!(
        "Initializing miner, enabled_chains={:?}, midstate_count={}",
        enabled_chains, midstate_count,
    );
    // build all hash chain managers
    for hashboard_idx in enabled_chains.iter() {
        // build hashchain_node for statistics
        let hashchain_node = Arc::new(HashChainNode {
            hashboard_idx: *hashboard_idx,
            ..Default::default()
        });
        let (work_generator, work_solution) = work_solver_builder
            .branch(hashchain_node.clone())
            .await
            .split();
        let hm = HashChainManager {
            // TODO: create a new substructure of the miner that will hold all gpio and
            // "physical-insertion" detection data. This structure will be persistent in
            // between restarts and will enable early notification that there is no hashboard
            // inserted (instead find out at mining-time).
            gpio_mgr: gpio::ControlPinManager::new(),
            voltage_ctrl_backend: voltage_ctrl_backend.clone(),
            hashboard_idx: *hashboard_idx,
            midstate_count: MidstateCount::new(midstate_count),
            asic_difficulty: config::ASIC_DIFFICULTY,
            work_generator,
            work_solution,
            shutdown: shutdown.clone(),
            runtime: None,
            params: HashChainParams {
                pll_frequency: vec![pll_frequency; MAX_CHIPS_ON_CHAIN],
                voltage,
            },
            monitor_tx: monitor::Monitor::register_hashchain(monitor.clone(), *hashboard_idx).await,
        };
        hms.push(hm);
    }
    // start everything
    for mut hm in hms.drain(..) {
        hm.start().await.expect("failed to start hashchain");
    }
}

/// Represents raw solution from the Antminer S9
#[derive(Clone, Debug)]
pub struct Solution {
    /// Actual nonce
    nonce: u32,
    /// Index of a midstate that corresponds to the found nonce
    midstate_idx: usize,
    /// Index of a solution (if multiple were found)
    solution_idx: usize,
    /// Hardware specific solution identifier
    pub hardware_id: u32,
}

impl hal::BackendSolution for Solution {
    #[inline]
    fn nonce(&self) -> u32 {
        self.nonce
    }

    #[inline]
    fn midstate_idx(&self) -> usize {
        self.midstate_idx
    }

    #[inline]
    fn solution_idx(&self) -> usize {
        self.solution_idx
    }

    #[inline]
    fn target(&self) -> &ii_bitcoin::Target {
        &ASIC_TARGET
    }
}

#[derive(Debug, WorkSolverNode)]
pub struct Backend {
    #[member_work_solver_stats]
    work_solver_stats: stats::BasicWorkSolver,
    pll_frequency: usize,
    voltage: f32,
}

impl Backend {
    pub fn new() -> Self {
        Self {
            work_solver_stats: Default::default(),
            pll_frequency: config::DEFAULT_PLL_FREQUENCY,
            voltage: config::DEFAULT_VOLTAGE,
        }
    }
}

impl hal::Backend for Backend {
    const DEFAULT_MIDSTATE_COUNT: usize = config::DEFAULT_MIDSTATE_COUNT;
    const DEFAULT_HASHRATE_INTERVAL: Duration = config::DEFAULT_HASHRATE_INTERVAL;
    const JOB_TIMEOUT: Duration = config::JOB_TIMEOUT;

    fn add_args<'a, 'b>(&self, app: clap::App<'a, 'b>) -> clap::App<'a, 'b> {
        app.arg(
            clap::Arg::with_name("disable-asic-boost")
                .long("disable-asic-boost")
                .help("Disable ASIC boost (use just one midstate)")
                .required(false),
        )
        .arg(
            clap::Arg::with_name("pll-frequency")
                .long("pll-frequency")
                .help("Set chip frequency")
                .required(false)
                .takes_value(true),
        )
        .arg(
            clap::Arg::with_name("voltage")
                .long("voltage")
                .help("Set chip voltage")
                .required(false)
                .takes_value(true),
        )
    }

    fn init(&mut self, args: &clap::ArgMatches) {
        // Set just 1 midstate if user requested disabling asicboost
        if args.is_present("disable-asic-boost") {
            runtime_config::set_midstate_count(1);
        }
        if args.is_present("pll-frequency") {
            self.pll_frequency = args
                .value_of("pll-frequency")
                .expect("argument missing")
                .parse::<usize>()
                .expect("parser failed");
        }
        if args.is_present("voltage") {
            self.voltage = args
                .value_of("voltage")
                .expect("argument missing")
                .parse::<f32>()
                .expect("parser failed");
        }
    }

    fn run(self: Arc<Self>, work_solver_builder: work::SolverBuilder, shutdown: shutdown::Sender) {
        tokio::spawn(start_miner(
            vec![config::S9_HASHBOARD_INDEX],
            work_solver_builder,
            shutdown,
            runtime_config::get_midstate_count(),
            self.pll_frequency,
            power::Voltage::from_volts(self.voltage),
        ));
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Bitmain Antminer S9")
    }
}

/// Helper method that calculates baud rate clock divisor value for the specified baud rate.
///
/// The calculation follows the same scheme for the hashing chips as well as for the FPGA IP core
///
/// * `baud_rate` - requested baud rate
/// * `base_clock_hz` - base clock for the UART peripheral
/// * `base_clock_div` - divisor for the base clock
/// Return a baudrate divisor and actual baud rate or an error
fn calc_baud_clock_div(
    baud_rate: usize,
    base_clock_hz: usize,
    base_clock_div: usize,
) -> error::Result<(usize, usize)> {
    const MAX_BAUD_RATE_ERR_PERC: usize = 5;
    // The actual calculation is:
    // base_clock_hz / (base_clock_div * baud_rate) - 1
    // We have to mathematically round the calculated divisor in fixed point arithmethic
    let baud_div = (10 * base_clock_hz / (base_clock_div * baud_rate) + 5) / 10 - 1;
    let actual_baud_rate = base_clock_hz / (base_clock_div * (baud_div + 1));

    //
    let baud_rate_diff = if actual_baud_rate > baud_rate {
        actual_baud_rate - baud_rate
    } else {
        baud_rate - actual_baud_rate
    };
    // the baud rate has to be within a few percents
    if baud_rate_diff > (MAX_BAUD_RATE_ERR_PERC * baud_rate / 100) {
        Err(ErrorKind::BaudRate(format!(
            "requested {} baud, resulting {} baud",
            baud_rate, actual_baud_rate
        )))?
    }
    Ok((baud_div, actual_baud_rate))
}

/// Helper method to calculate time to finish one piece of work
///
/// * `n_midstates` - number of midstates
/// * `pll_frequency` - frequency of chip in Hz
/// Return a number of seconds.
///
/// The formula for work_delay is:
///
///   work_delay = space_size_of_one_work / computation_speed; [sec, hashes, hashes_per_sec]
///
/// In our case it would be
///
///   work_delay = n_midstates * 2^32 / (freq * num_chips * cores_per_chip)
///
/// Unfortunately the space is not divided evenly, some nonces get never computed.
/// The current conjecture is that nonce space is divided by chip/core address,
/// ie. chip number 0x1a iterates all nonces 0x1axxxxxx. That's 6 bits of chip_address
/// and 7 bits of core_address. Putting it all together:
///
///   work_delay = n_midstates * num_chips * cores_per_chip * 2^(32 - 7 - 6) / (freq * num_chips * cores_per_chip)
///
/// Simplify:
///
///   work_delay = n_midstates * 2^19 / freq
///
/// Last but not least, we apply fudge factor of 0.9 and send work 11% faster to offset
/// delays when sending out/generating work/chips not getting proper work...:
///
///   work_delay = 0.9 * n_midstates * 2^19 / freq
fn calculate_work_delay_for_pll(n_midstates: usize, pll_frequency: usize) -> f64 {
    let space_size_per_core: u64 = 1 << 19;
    0.9 * (n_midstates as u64 * space_size_per_core) as f64 / pll_frequency as f64
}

/// Helper method to convert seconds to FPGA ticks suitable to be written
/// to `WORK_TIME` FPGA register.
///
/// Returns number of ticks.
fn secs_to_fpga_ticks(secs: f64) -> u32 {
    (secs * FPGA_IPCORE_F_CLK_SPEED_HZ as f64) as u32
}
