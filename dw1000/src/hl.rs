//! High-level interface to the DW1000
//!
//! The entry point to this API is the [DW1000] struct. Please refer to the
//! documentation there for more details.
//!
//! This module implements a high-level interface to the DW1000. This is the
//! recommended way to access the DW1000 using this crate, unless you need the
//! greater flexibility provided by the [register-level interface].
//!
//! [register-level interface]: ../ll/index.html


use core::{
    fmt,
    num::Wrapping,
    convert::TryInto,
};

use embedded_hal::{
    blocking::spi,
    digital::v2::OutputPin,
};
use nb;
use ssmarshal;
#[allow(unused_imports)]
use micromath::F32Ext;
use fixed::traits::LossyInto;

use crate::{
    ll,
    mac,
    time::{
        Duration,
        Instant,
    },
    configs::{
        TxConfig,
        RxConfig,
        SfdSequence,
        BitRate,
        MaximumFrameLength
    },
};

/// Entry point to the DW1000 driver API
pub struct DW1000<SPI, CS, State> {
    ll:    ll::DW1000<SPI, CS>,
    seq:   Wrapping<u8>,
    state: State,
    max_frame_len: MaximumFrameLength,
}

impl<SPI, CS> DW1000<SPI, CS, Uninitialized>
    where
        SPI: spi::Transfer<u8> + spi::Write<u8>,
        CS:  OutputPin,
{
    /// Create a new instance of `DW1000`
    ///
    /// Requires the SPI peripheral and the chip select pin that are connected
    /// to the DW1000.
    pub fn new(
        spi        : SPI,
        chip_select: CS,
    )
        -> Self
    {
        DW1000 {
            ll:    ll::DW1000::new(spi, chip_select),
            seq:   Wrapping(0),
            state: Uninitialized,
            max_frame_len: MaximumFrameLength::Standard127
        }
    }

    /// Initialize the DW1000
    ///
    /// The DW1000's default configuration is somewhat inconsistent, and the
    /// user manual (section 2.5.5) has a long list of default configuration
    /// values that should be changed to guarantee everything works correctly.
    /// This method does just that.
    ///
    /// Please note that this method assumes that you kept the default
    /// configuration. It is generally recommended not to change configuration
    /// before calling this method.
    pub fn init(mut self, max_frame_len: MaximumFrameLength) -> Result<DW1000<SPI, CS, Ready>, Error<SPI, CS>> {
        // Set AGC_TUNE1. See user manual, section 2.5.5.1.
        self.ll.agc_tune1().write(|w| w.value(0x8870))?;

        // Set AGC_TUNE2. See user manual, section 2.5.5.2.
        self.ll.agc_tune2().write(|w| w.value(0x2502A907))?;

        // Set DRX_TUNE2. See user manual, section 2.5.5.3.
        self.ll.drx_tune2().write(|w| w.value(0x311A002D))?;

        // Set NTM. See user manual, section 2.5.5.4. This improves performance
        // in line-of-sight conditions, but might not be the best choice if non-
        // line-of-sight performance is important.
        self.ll.lde_cfg1().modify(|_, w| w.ntm(0xD))?;

        // Set LDE_CFG2. See user manual, section 2.5.5.5.
        self.ll.lde_cfg2().write(|w| w.value(0x1607))?;

        // Set TX_POWER. See user manual, section 2.5.5.6.
        self.ll.tx_power().write(|w| w.value(0x0E082848))?;

        // Set RF_TXCTRL. See user manual, section 2.5.5.7.
        self.ll.rf_txctrl().modify(|_, w|
            w
                .txmtune(0b1111)
                .txmq(0b111)
        )?;

        // Set TC_PGDELAY. See user manual, section 2.5.5.8.
        self.ll.tc_pgdelay().write(|w| w.value(0xC0))?;

        // Set FS_PLLTUNE. See user manual, section 2.5.5.9.
        self.ll.fs_plltune().write(|w| w.value(0xBE))?;

        // Set LDELOAD. See user manual, section 2.5.5.10.
        self.ll.pmsc_ctrl0().modify(|_, w| w.sysclks(0b01))?;
        self.ll.otp_ctrl().modify(|_, w| w.ldeload(0b1))?;
        while self.ll.otp_ctrl().read()?.ldeload() == 0b1 {}
        self.ll.pmsc_ctrl0().modify(|_, w| w.sysclks(0b00))?;

        // Set LDOTUNE. See user manual, section 2.5.5.11.
        self.ll.otp_addr().write(|w| w.value(0x004))?;
        self.ll.otp_ctrl().modify(|_, w|
            w
                .otprden(0b1)
                .otpread(0b1)
        )?;
        while self.ll.otp_ctrl().read()?.otpread() == 0b1 {}
        let ldotune_low = self.ll.otp_rdat().read()?.value();
        if ldotune_low != 0 {
            self.ll.otp_addr().write(|w| w.value(0x005))?;
            self.ll.otp_ctrl().modify(|_, w|
                w
                    .otprden(0b1)
                    .otpread(0b1)
            )?;
            while self.ll.otp_ctrl().read()?.otpread() == 0b1 {}
            let ldotune_high = self.ll.otp_rdat().read()?.value();

            let ldotune = ldotune_low as u64 | (ldotune_high as u64) << 32;
            self.ll.ldotune().write(|w| w.value(ldotune))?;
        }

        if max_frame_len == MaximumFrameLength::Decawave1023 {
            self.ll.sys_cfg().modify(|_, w|
                w
                    .phr_mode(0b11)
            )?;
        }

        Ok(DW1000 {
            ll:    self.ll,
            seq:   self.seq,
            state: Ready,
            max_frame_len
        })
    }
}

impl<SPI, CS> DW1000<SPI, CS, Ready>
    where
        SPI: spi::Transfer<u8> + spi::Write<u8>,
        CS:  OutputPin,
{
    /// Sets the RX and TX antenna delays
    pub fn set_antenna_delay(&mut self, rx_delay: u16, tx_delay: u16)
        -> Result<(), Error<SPI, CS>>
    {
        self.ll
            .lde_rxantd()
            .write(|w| w.value(rx_delay))?;
        self.ll
            .tx_antd()
            .write(|w| w.value(tx_delay))?;

        Ok(())
    }

    /// Sets the network id and address used for sending and receiving
    pub fn set_address(&mut self, pan_id: mac::PanId, addr: mac::ShortAddress)
        -> Result<(), Error<SPI, CS>>
    {
        self.ll
            .panadr()
            .write(|w|
                w
                    .pan_id(pan_id.0)
                    .short_addr(addr.0)
            )?;

        Ok(())
    }

    /// Send an IEEE 802.15.4 MAC frame
    ///
    /// The `data` argument is wrapped into an IEEE 802.15.4 MAC frame and sent
    /// to `destination`.
    ///
    /// This operation can be delayed to aid in distance measurement, by setting
    /// `delayed_time` to `Some(instant)`. If you want to send the frame as soon
    /// as possible, just pass `None` instead.
    ///
    /// The config parameter struct allows for setting the channel, bitrate, and
    /// more. This configuration needs to be the same as the configuration used
    /// by the receiver, or the message may not be received.
    /// The defaults are a sane starting point.
    ///
    /// This method starts the transmission and returns immediately thereafter.
    /// It consumes this instance of `DW1000` and returns another instance which
    /// is in the `Sending` state, and can be used to wait for the transmission
    /// to finish and check its result.
    pub fn send(mut self,
                data:         &[u8],
                destination:  mac::Address,
                delayed_time: Option<Instant>,
                config: TxConfig,
    )
                -> Result<DW1000<SPI, CS, Sending>, Error<SPI, CS>>
    {
        let seq = self.seq.0;
        self.seq += Wrapping(1);
        let frame = mac::Frame {
            header: mac::Header {
                frame_type:      mac::FrameType::Data,
                version:         mac::FrameVersion::Ieee802154_2006,
                security:        mac::Security::None,
                frame_pending:   false,
                ack_request:     false,
                pan_id_compress: false,
                destination:     destination,
                source:          self.get_address()?,
                seq:             seq,
            },
            content: mac::FrameContent::Data,
            payload: data,
            footer: [0; 2],
        };
        let mut buffer = [0u8; 1200];
        //buffer[0] = 0x09 | 0b1000_000;
        let len = frame.encode(&mut buffer, mac::WriteFooter::No);
        self.send_raw(&buffer[0..len], delayed_time, config)
    }

    /// Send raw data
    pub fn send_raw(mut self,
                data:         &[u8],
                delayed_time: Option<Instant>,
                config: TxConfig,
    )
                -> Result<DW1000<SPI, CS, Sending>, Error<SPI, CS>>
    {
        // Clear event counters
        self.ll.evc_ctrl().write(|w| w.evc_clr(0b1))?;
        while self.ll.evc_ctrl().read()?.evc_clr() == 0b1 {}

        // (Re-)Enable event counters
        self.ll.evc_ctrl().write(|w| w.evc_en(0b1))?;
        while self.ll.evc_ctrl().read()?.evc_en() == 0b1 {}

        // Sometimes, for unknown reasons, the DW1000 gets stuck in RX mode.
        // Starting the transmitter won't get it to enter TX mode, which means
        // all subsequent send operations will fail. Let's disable the
        // transceiver and force the chip into IDLE mode to make sure that
        // doesn't happen.
        self.force_idle()?;

        delayed_time.map(|time| {
            self.ll
                .dx_time()
                .write(|w|
                    w.value(time.value())
                )
        });

        // Prepare transmitter
        let len = data.len();
        // self.ll
        //     .tx_buffer()
        //     .write(|w| {
        //         unsafe {
        //             core::ptr::copy(
        //                 data.as_ptr(),
        //                 w.data().as_mut_ptr(),
        //                 len);
        //         }
        //         w
        //     })?;
        self.ll.write_tx_buffer(data)?;

        // let tfle: u8 = match self.max_frame_len {
        //     MaximumFrameLength::Standard127 => { 0 },
        //     MaximumFrameLength::Decawave1023 => { (((len as u16 + 2) >> 7) & 0b111) as u8 },
        // };
        let tfle = (((len as u16 + 2) >> 7) & 0b111) as u8;
        self.ll
            .tx_fctrl()
            .modify(|_, w| {
                let tflen = len as u8 + 2;
                w
                    .tflen(tflen) // data length + two-octet CRC
                    .tfle(tfle)   // no non-standard length extension
                    .txboffs(0)   // no offset in TX_BUFFER
                    .txbr(config.bitrate as u8) // configured bitrate
                    .tr(config.ranging_enable as u8) // configured ranging bit
                    .txprf(config.pulse_repetition_frequency as u8) // configured PRF
                    .txpsr(((config.preamble_length as u8) & 0b1100) >> 2) // first two bits of configured preamble length
                    .pe((config.preamble_length as u8) & 0b0011) // last two bits of configured preamble length
            })?;

        // Set the channel and sfd settings
        self.ll
            .chan_ctrl()
            .modify(|_, w| {
                w
                    .tx_chan(config.channel as u8)
                    .rx_chan(config.channel as u8)
                    .dwsfd((config.sfd_sequence == SfdSequence::Decawave || config.sfd_sequence == SfdSequence::DecawaveAlt) as u8)
                    .rxprf(config.pulse_repetition_frequency as u8)
                    .tnssfd((config.sfd_sequence == SfdSequence::User || config.sfd_sequence == SfdSequence::DecawaveAlt) as u8)
                    .rnssfd((config.sfd_sequence == SfdSequence::User || config.sfd_sequence == SfdSequence::DecawaveAlt) as u8)
                    .tx_pcode(config.channel.get_recommended_preamble_code(config.pulse_repetition_frequency))
                    .rx_pcode(config.channel.get_recommended_preamble_code(config.pulse_repetition_frequency))
            })?;

        match config.sfd_sequence {
            SfdSequence::IEEE => {}, // IEEE has predefined sfd lengths and the register has no effect.
            SfdSequence::Decawave => self.ll.sfd_length().write(|w| w.value(8))?, // This isn't entirely necessary as the Decawave8 settings in chan_ctrl already force it to 8
            SfdSequence::DecawaveAlt => self.ll.sfd_length().write(|w| w.value(16))?, // Set to 16
            SfdSequence::User => {}, // Users are responsible for setting the lengths themselves
        }

        // Tune for the correct channel
        self.ll.rf_txctrl().write(|w| w.value(config.channel.get_recommended_rf_txctrl()))?;
        self.ll.tc_pgdelay().write(|w| w.value(config.channel.get_recommended_tc_pgdelay()))?;
        self.ll.fs_pllcfg().write(|w| w.value(config.channel.get_recommended_fs_pllcfg()))?;
        self.ll.fs_plltune().write(|w| w.value(config.channel.get_recommended_fs_plltune()))?;

        // Todo: Power control (register 0x1E)

        // Start transmission
        self.ll
            .sys_ctrl()
            .modify(|_, w|
                if delayed_time.is_some() { w.txdlys(0b1) } else { w }
                    .txstrt(0b1)
            )?;

        Ok(DW1000 {
            ll:    self.ll,
            seq:   self.seq,
            state: Sending { finished: false },
            max_frame_len: self.max_frame_len
        })
    }

    /// Attempt to receive an IEEE 802.15.4 MAC frame
    ///
    /// Initializes the receiver. The method consumes this instance of `DW1000`
    /// and returns another instance which is in the `Receiving` state, and can
    /// be used to wait for a message.
    ///
    /// The config parameter allows for the configuration of bitrate, channel
    /// and more. Make sure that the values used are the same as of the frames
    /// that are transmitted. The default works with the TxConfig's default and
    /// is a sane starting point.
    pub fn receive(mut self, config: RxConfig)
        -> Result<DW1000<SPI, CS, Receiving>, Error<SPI, CS>>
    {
        // For unknown reasons, the DW1000 gets stuck in RX mode without ever
        // receiving anything, after receiving one good frame. Reset the
        // receiver to make sure its in a valid state before attempting to
        // receive anything.
        self.ll
            .pmsc_ctrl0()
            .modify(|_, w|
                w.softreset(0b1110) // reset receiver
            )?;
        self.ll
            .pmsc_ctrl0()
            .modify(|_, w|
                w.softreset(0b1111) // clear reset
            )?;

        // We're already resetting the receiver in the previous step, and that's
        // good enough to make my example program that's both sending and
        // receiving work very reliably over many hours (that's not to say it
        // becomes unreliable after those hours, that's just when my test
        // stopped). However, I've seen problems with an example program that
        // only received, never sent, data. That got itself into some weird
        // state where it couldn't receive anymore.
        // I suspect that's because that example didn't have the following line
        // of code, while the send/receive example had that line of code, being
        // called from `send`.
        // While I haven't, as of this writing, run any hours-long tests to
        // confirm this does indeed fix the receive-only example, it seems
        // (based on my eyeball-only measurements) that the RX/TX example is
        // dropping fewer frames now.
        self.force_idle()?;

        if config.frame_filtering {
            self.ll
                .sys_cfg()
                .modify(|_, w|
                    w
                        .ffen(0b1) // enable frame filtering
                        .ffab(0b1) // receive beacon frames
                        .ffad(0b1) // receive data frames
                        .ffaa(0b1) // receive acknowledgement frames
                        .ffam(0b1) // receive MAC command frames
                )?;
        }
        else {
            self.ll
                .sys_cfg()
                .modify(|_, w| w.ffen(0b0))?; // disable frame filtering
        }

        // Set PLLLDT bit in EC_CTRL. According to the documentation of the
        // CLKPLL_LL bit in SYS_STATUS, this bit needs to be set to ensure the
        // reliable operation of the CLKPLL_LL bit. Since I've seen that bit
        // being set, I want to make sure I'm not just seeing crap.
        self.ll
            .ec_ctrl()
            .modify(|_, w|
                w.pllldt(0b1)
            )?;

        // Now that PLLLDT is set, clear all bits in SYS_STATUS that depend on
        // it for reliable operation. After that is done, these bits should work
        // reliably.
        self.ll
            .sys_status()
            .write(|w|
                w
                    .cplock(0b1)
                    .clkpll_ll(0b1)
            )?;

        // Apply the config
        self.ll.chan_ctrl().modify(|_, w| {
            w
                .tx_chan(config.channel as u8)
                .rx_chan(config.channel as u8)
                .dwsfd((config.sfd_sequence == SfdSequence::Decawave || config.sfd_sequence == SfdSequence::DecawaveAlt) as u8)
                .rxprf(config.pulse_repetition_frequency as u8)
                .tnssfd((config.sfd_sequence == SfdSequence::User || config.sfd_sequence == SfdSequence::DecawaveAlt) as u8)
                .rnssfd((config.sfd_sequence == SfdSequence::User || config.sfd_sequence == SfdSequence::DecawaveAlt) as u8)
                .tx_pcode(config.channel.get_recommended_preamble_code(config.pulse_repetition_frequency))
                .rx_pcode(config.channel.get_recommended_preamble_code(config.pulse_repetition_frequency))
        })?;

        match config.sfd_sequence {
            SfdSequence::IEEE => {}, // IEEE has predefined sfd lengths and the register has no effect.
            SfdSequence::Decawave => self.ll.sfd_length().write(|w| w.value(8))?, // This isn't entirely necessary as the Decawave8 settings in chan_ctrl already force it to 8
            SfdSequence::DecawaveAlt => self.ll.sfd_length().write(|w| w.value(16))?, // Set to 16
            SfdSequence::User => {}, // Users are responsible for setting the lengths themselves
        }

        // Set general tuning
        self.ll.drx_tune0b().write(|w| w.value(config.bitrate.get_recommended_drx_tune0b(config.sfd_sequence)))?;
        self.ll.drx_tune1a().write(|w| w.value(config.pulse_repetition_frequency.get_recommended_drx_tune1a()))?;
        let drx_tune1b = config.expected_preamble_length.get_recommended_drx_tune1b(config.bitrate)?;
        self.ll.drx_tune1b().write(|w| w.value(drx_tune1b))?;
        let drx_tune2 = config.pulse_repetition_frequency.get_recommended_drx_tune2(config.expected_preamble_length.get_recommended_pac_size())?;
        self.ll.drx_tune2().write(|w| w.value(drx_tune2))?;
        self.ll.drx_tune4h().write(|w| w.value(config.expected_preamble_length.get_recommended_dxr_tune4h()))?;

        // Set channel tuning
        self.ll.rf_rxctrlh().write(|w| w.value(config.channel.get_recommended_rf_rxctrlh()))?;
        self.ll.fs_pllcfg().write(|w| w.value(config.channel.get_recommended_fs_pllcfg()))?;
        self.ll.fs_plltune().write(|w| w.value(config.channel.get_recommended_fs_plltune()))?;

        // Set the rx bitrate
        self.ll.sys_cfg().modify(|_, w| w.rxm110k((config.bitrate == BitRate::Kbps110) as u8))?;

        self.ll
            .sys_ctrl()
            .modify(|_, w|
                w.rxenab(0b1)
            )?;

        Ok(DW1000 {
            ll:    self.ll,
            seq:   self.seq,
            state: Receiving {
                finished: false,
                used_config: config
            },
            max_frame_len: self.max_frame_len,
        })
    }

    /// Enables transmit interrupts for the events that `wait` checks
    ///
    /// Overwrites any interrupt flags that were previously set.
    pub fn enable_tx_interrupts(&mut self)
        -> Result<(), Error<SPI, CS>>
    {
        self.ll.sys_mask().modify(|_, w| w.mtxfrs(0b1))?;
        Ok(())
    }

    /// Enables receive interrupts for the events that `wait` checks
    ///
    /// Overwrites any interrupt flags that were previously set.
    pub fn enable_rx_interrupts(&mut self)
        -> Result<(), Error<SPI, CS>>
    {
        self.ll()
            .sys_mask()
            .modify(|_, w|
                w
                    .mrxdfr(0b1)
                    .mrxfce(0b1)
                    .mrxphe(0b1)
                    .mrxrfsl(0b1)
                    .mrxrfto(0b1)
                    .mrxovrr(0b1)
                    .mrxpto(0b1)
                    .mrxsfdto(0b1)
                    .maffrej(0b1)
                   // .mldedone(0b1) // user manual page 89: RXDFR won't be fired until LDEDONE
            )?;

        Ok(())
    }

    /// Disables all interrupts
    pub fn disable_interrupts(&mut self)
        -> Result<(), Error<SPI, CS>>
    {
        self.ll.sys_mask().write(|w| w)?;
        Ok(())
    }

    /// Configures the gpio pins to operate as LED output.
    ///
    /// - Note: This means that the function of the gpio pins change
    /// - Note: Both the kilohertz and debounce clock will be turned on or off
    /// ---
    /// - RXOKLED will change GPIO0
    /// - SFDLED will change GPIO1
    /// - RXLED will change GPIO2
    /// - TXLED will change GPIO3
    ///
    /// blink_time is in units of 14 ms
    pub fn configure_leds(
        &mut self,
        enable_rx_ok: bool,
        enable_sfd: bool,
        enable_rx: bool,
        enable_tx: bool,
        blink_time: u8)
        -> Result<(), Error<SPI, CS>> {
        // Turn on the timer that will control the blinking (The debounce clock)
        self.ll.pmsc_ctrl0().modify(|_, w| {
            w
                .gpdce((enable_rx_ok || enable_sfd || enable_rx || enable_tx) as u8)
                .khzclken((enable_rx_ok || enable_sfd || enable_rx || enable_tx) as u8)
        })?;

        // Turn on the led blinking
        self.ll.pmsc_ledc().modify(|_, w| {
           w
               .blnken((enable_rx_ok || enable_sfd || enable_rx || enable_tx) as u8)
               .blink_tim(blink_time)
        })?;

        // Set the proper gpio mode
        self.ll.gpio_mode().modify(|_, w| {
            w
                .msgp0(enable_rx_ok as u8)
                .msgp1(enable_sfd as u8)
                .msgp2(enable_rx as u8)
                .msgp3(enable_tx as u8)
        })?;

        Ok(())
    }
}

impl<SPI, CS> DW1000<SPI, CS, Sending>
    where
        SPI: spi::Transfer<u8> + spi::Write<u8>,
        CS:  OutputPin,
{
    /// Wait for the transmission to finish
    ///
    /// This method returns an `nb::Result` to indicate whether the transmission
    /// has finished, or whether it is still ongoing. You can use this to busily
    /// wait for the transmission to finish, for example using `nb`'s `block!`
    /// macro, or you can use it in tandem with [`DW1000::enable_tx_interrupts`]
    /// and the DW1000 IRQ output to wait in a more energy-efficient manner.
    ///
    /// Handling the DW1000's IRQ output line is out of the scope of this
    /// driver, but please note that if you're using the DWM1001 module or
    /// DWM1001-Dev board, that the `dwm1001` crate has explicit support for
    /// this.
    pub fn wait(&mut self)
        -> nb::Result<(), Error<SPI, CS>>
    {
        // Check Half Period Warning Counter. If this is a delayed transmission,
        // this will indicate that the delay was too short, and the frame was
        // sent too late.
        let evc_hpw = self.ll
            .evc_hpw()
            .read()
            .map_err(|error| nb::Error::Other(Error::Spi(error)))?
            .value();
        if evc_hpw != 0 {
            return Err(nb::Error::Other(Error::DelayedSendTooLate));
        }

        // Check Transmitter Power-Up Warning Counter. If this is a delayed
        // transmission, this indicates that the transmitter was still powering
        // up while sending, and the frame preamble might not have transmit
        // correctly.
        let evc_tpw = self.ll
            .evc_tpw()
            .read()
            .map_err(|error| nb::Error::Other(Error::Spi(error)))?
            .value();
        if evc_tpw != 0 {
            return Err(nb::Error::Other(Error::DelayedSendPowerUpWarning));
        }

        // ATTENTION:
        // If you're changing anything about which SYS_STATUS flags are being
        // checked in this method, also make sure to update `enable_interrupts`.
        let sys_status = self.ll
            .sys_status()
            .read()
            .map_err(|error| nb::Error::Other(Error::Spi(error)))?;

        // Has the frame been sent?
        if sys_status.txfrs() == 0b0 {
            // Frame has not been sent
            return Err(nb::Error::WouldBlock);
        }

        // Frame sent
        self.reset_flags()
            .map_err(|error| nb::Error::Other(error))?;
        self.state.finished = true;

        Ok(())
    }

    /// Finishes sending and returns to the `Ready` state
    ///
    /// If the send operation has finished, as indicated by `wait`, this is a
    /// no-op. If the send operation is still ongoing, it will be aborted.
    pub fn finish_sending(mut self)
        -> Result<DW1000<SPI, CS, Ready>, (Self, Error<SPI, CS>)>
    {
        if !self.state.finished {
            // Can't use `map_err` and `?` here, as the compiler will complain
            // about `self` moving into the closure.
            match self.force_idle() {
                Ok(())     => (),
                Err(error) => return Err((self, error)),
            }
            match self.reset_flags() {
                Ok(())     => (),
                Err(error) => return Err((self, error)),
            }
        }

        Ok(DW1000 {
            ll:    self.ll,
            seq:   self.seq,
            state: Ready,
            max_frame_len: self.max_frame_len,
        })
    }

    fn reset_flags(&mut self) -> Result<(), Error<SPI, CS>> {
        self.ll
            .sys_status()
            .write(|w|
                w
                    .txfrb(0b1) // Transmit Frame Begins
                    .txprs(0b1) // Transmit Preamble Sent
                    .txphs(0b1) // Transmit PHY Header Sent
                    .txfrs(0b1) // Transmit Frame Sent
            )?;

        Ok(())
    }
}

impl<SPI, CS> DW1000<SPI, CS, Receiving>
    where
        SPI: spi::Transfer<u8> + spi::Write<u8>,
        CS:  OutputPin,
{
    /// Wait for receive operation to finish
    ///
    /// This method returns an `nb::Result` to indicate whether the transmission
    /// has finished, or whether it is still ongoing. You can use this to busily
    /// wait for the transmission to finish, for example using `nb`'s `block!`
    /// macro, or you can use it in tandem with [`DW1000::enable_rx_interrupts`]
    /// and the DW1000 IRQ output to wait in a more energy-efficient manner.
    ///
    /// Handling the DW1000's IRQ output line is out of the scope of this
    /// driver, but please note that if you're using the DWM1001 module or
    /// DWM1001-Dev board, that the `dwm1001` crate has explicit support for
    /// this.
    pub fn wait<'b>(&mut self, buffer: &'b mut [u8])
        -> nb::Result<(Message<'b>, ll::sys_status::R), Error<SPI, CS>>
    {
        // ATTENTION:
        // If you're changing anything about which SYS_STATUS flags are being
        // checked in this method, also make sure to update `enable_interrupts`.
        let sys_status = self.ll()
            .sys_status()
            .read()
            .map_err(|error| nb::Error::Other(Error::Spi(error)))?;

        // Is a frame ready?
        if sys_status.rxdfr() == 0b0 {
            // No frame ready. Check for errors.
            if sys_status.rxfce() == 0b1 {
                return Err(nb::Error::Other(Error::Fcs));
            }
            if sys_status.rxphe() == 0b1 {
                return Err(nb::Error::Other(Error::Phy));
            }
            if sys_status.rxrfsl() == 0b1 {
                return Err(nb::Error::Other(Error::ReedSolomon));
            }
            if sys_status.rxrfto() == 0b1 {
                return Err(nb::Error::Other(Error::FrameWaitTimeout));
            }
            if sys_status.rxovrr() == 0b1 {
                return Err(nb::Error::Other(Error::Overrun));
            }
            if sys_status.rxpto() == 0b1 {
                return Err(nb::Error::Other(Error::PreambleDetectionTimeout));
            }
            if sys_status.rxsfdto() == 0b1 {
                return Err(nb::Error::Other(Error::SfdTimeout));
            }
            if sys_status.affrej() == 0b1 {
                return Err(nb::Error::Other(Error::FrameFilteringRejection))
            }
            // Some error flags that sound like valid errors aren't checked here,
            // because experience has shown that they seem to occur spuriously
            // without preventing a good frame from being received. Those are:
            // - LDEERR: Leading Edge Detection Processing Error
            // - RXPREJ: Receiver Preamble Rejection

            // No errors detected. That must mean the frame is just not ready
            // yet.
            return Err(nb::Error::WouldBlock);
        }

        // Frame is ready. Continue.
        // Clear all receive status bits [from deca_device.c]
        self.ll()
            .sys_status()
            .write(|w|
                       w
                           .rxdfr(0b1)
                           .rxfcg(0b1)
                           .rxprd(0b1)
                           .rxsfdd(0b1)
                           .rxphd(0b1)

            )
            .map_err(|error| nb::Error::Other(Error::Spi(error)))?;


        // Wait until LDE processing is done. Before this is finished, the RX
        // time stamp is not available.
        if sys_status.ldedone() == 0b0 {
            return Err(nb::Error::WouldBlock);
        }
        let rx_time = self.ll()
            .rx_time()
            .read()
            .map_err(|error| nb::Error::Other(Error::Spi(error)))?
            .rx_stamp();

        // `rx_time` comes directly from the register, which should always
        // contain a 40-bit timestamp. Unless the hardware or its documentation
        // are buggy, the following should never panic.
        let rx_time = Instant::new(rx_time).unwrap();

        // Read received frame
        let rx_finfo = self.ll()
            .rx_finfo()
            .read()
            .map_err(|error| nb::Error::Other(Error::Spi(error)))?;
        // Read down below, only required number of bytes
        // let rx_buffer = self.ll()
        //     .rx_buffer()
        //     .read()
        //     .map_err(|error| nb::Error::Other(Error::Spi(error)))?;

        let len: usize = match self.max_frame_len {
            MaximumFrameLength::Standard127 => {
                rx_finfo.rxflen() as usize
            },
            MaximumFrameLength::Decawave1023 => {
                (((rx_finfo.rxfle() as u16) << 7) | rx_finfo.rxflen() as u16) as usize
            }
        };

        if buffer.len() < len {
            return Err(nb::Error::Other(
                Error::BufferTooSmall { required_len: len }
            ))
        }
        self.ll.read_rx_buffer(&mut buffer[0..len]).map_err(|error| nb::Error::Other(Error::Spi(error)))?;

        //buffer[..len].copy_from_slice(&rx_buffer.data()[..len]);

        let frame = mac::Frame::decode(&buffer[..len], true)
            .map_err(|error| nb::Error::Other(Error::Frame(error)))?;

        // Reset status bits. This is not strictly necessary, but it helps, if
        // you have to inspect SYS_STATUS manually during debugging.
        self.ll()
            .sys_status()
            .write(|w|
                       w
                           .rxprd(0b1)   // Receiver Preamble Detected
                           .rxsfdd(0b1)  // Receiver SFD Detected
                           .ldedone(0b1) // LDE Processing Done
                           .rxphd(0b1)   // Receiver PHY Header Detected
                           .rxphe(0b1)   // Receiver PHY Header Error
                           .rxdfr(0b1)   // Receiver Data Frame Ready
                           .rxfcg(0b1)   // Receiver FCS Good
                           .rxfce(0b1)   // Receiver FCS Error
                           .rxrfsl(0b1)  // Receiver Reed Solomon Frame Sync Loss
                           .rxrfto(0b1)  // Receiver Frame Wait Timeout
                           .ldeerr(0b1)  // Leading Edge Detection Processing Error
                           .rxovrr(0b1)  // Receiver Overrun
                           .rxpto(0b1)   // Preamble Detection Timeout
                           .rxsfdto(0b1) // Receiver SFD Timeout
                           .rxrscs(0b1)  // Receiver Reed-Solomon Correction Status
                           .rxprej(0b1)  // Receiver Preamble Rejection
            )
            .map_err(|error| nb::Error::Other(Error::Spi(error)))?;

        Ok((Message { rx_time, frame }, sys_status))
    }

    fn calculate_luep(&mut self) -> Result<f32, Error<SPI, CS>> {
        let rx_time_register = self.ll().rx_time().read()?;
        let rx_fqual_register = self.ll().rx_fqual().read()?;
        let lde_cfg1_register = self.ll().lde_cfg1().read()?;

        let path_position: f32 =
            fixed::types::U10F6::from_le_bytes(
                rx_time_register.fp_index().to_le_bytes()
            ).lossy_into();

        // Calculate a new low threshold by taking 0.6 times the reported noise threshold from the
        // diagnostics. This new threshold is shown in red in Figure 5. Get existing noise threshold as the
        // multiplication of STD_NOISE from Register 12:00 and NTM from Register 2E:0806.
        let noise_threshold: u16 = rx_fqual_register.std_noise() * lde_cfg1_register.ntm() as u16;
        let new_low_threshold = (noise_threshold as f32 * 0.6) as u16;
        // From the integer part of the first path position, pathPosition,
        // form an analysis window of 16 samples back tracked from that index.
        const WINDOW_SIZE: usize = 16;
        let window_start = path_position as u16 - WINDOW_SIZE as u16;

        let mut cir_buffer = [0u8; WINDOW_SIZE*4 + 1];
        self.ll.cir(window_start * 4, &mut cir_buffer)?;
        let cir = &cir_buffer[1..];

        // To determine the number of peaks in the newly formed analysis window we take the difference of consecutive values.
        // We identify a peak when these differences change from positive to negative.

        // Calculate the amplitudes in the cir buffer
        let mut amplitudes = [0.0; WINDOW_SIZE];
        let mut peak_count = 0;
        for index in 0..WINDOW_SIZE {
            let real = u16::from_le_bytes(
                cir[index * 4..index * 4 + 2].try_into().unwrap()
            ) as f32;
            let imag = u16::from_le_bytes(
                cir[index * 4 + 2..index * 4 + 4].try_into().unwrap()
            ) as f32;

            amplitudes[index] = (real * real + imag * imag).sqrt();

            if index >= 2 && amplitudes[index - 1] > new_low_threshold as f32 {
                let previous_difference = amplitudes[index - 1] - amplitudes[index - 2];
                let current_difference = amplitudes[index] - amplitudes[index - 1];
                peak_count += (previous_difference.is_sign_positive() && current_difference.is_sign_negative()) as u8;
            }
        }

        Ok(peak_count as f32 / (WINDOW_SIZE / 2) as f32)
    }

    fn calculate_prnlos(&mut self) -> Result<f32, Error<SPI, CS>> {
        let rx_time_register = self.ll().rx_time().read()?;

        let path_position: f32 =
            fixed::types::U10F6::from_le_bytes(
                rx_time_register.fp_index().to_le_bytes()
            ).lossy_into();

        let peak_path_index: f32 = self.ll().lde_ppindx().read()?.value() as f32;

        let idiff = (path_position - peak_path_index).abs();
        if idiff <= 3.3 {
            Ok(0.0)
        } else if idiff < 6.0 {
            Ok(0.39178 * idiff - 1.31719)
        } else {
            Ok(1.0)
        }
    }

    fn calculate_mc(&mut self) -> Result<f32, Error<SPI, CS>> {
        let rx_time_register = self.ll().rx_time().read()?;
        let rx_fqual_register = self.ll().rx_fqual().read()?;

        let fp_ampl1: u16 = rx_time_register.fp_ampl1();
        let fp_ampl2: u16 = rx_fqual_register.fp_ampl2();
        let fp_ampl3: u16 = rx_fqual_register.fp_ampl3();
        let peak_path_amplitude: u16 = self.ll().lde_ppampl().read()?.value();

        Ok(fp_ampl1.max(fp_ampl2).max(fp_ampl3) as f32 / peak_path_amplitude as f32)
    }

    fn calculate_rssi(&mut self) -> Result<f32, Error<SPI, CS>> {
        let c = self.ll.rx_fqual().read()?.cir_pwr() as f32;
        let a = match self.state.used_config.pulse_repetition_frequency {
            crate::configs::PulseRepetitionFrequency::Mhz16 => 113.77,
            crate::configs::PulseRepetitionFrequency::Mhz64 => 121.74,
        };

        let data_rate = self.state.used_config.bitrate;
        let sfd_sequence = self.state.used_config.sfd_sequence;
        let n = self.ll.rx_finfo().read()?.rxpacc() as f32
            + sfd_sequence.get_rxpacc_adjustment(data_rate) as f32;

        Ok(10.0 * ((c * (2 << 17) as f32) / (n*n)).log10() - a)
    }

    /// Reads the quality of the received message.
    ///
    /// This must be called after the [`DW1000::wait`] function has successfully returned.
    pub fn read_rx_quality(&mut self) -> Result<RxQuality, Error<SPI, CS>> {
        if !self.state.finished {
            return Err(Error::RxNotFinished);
        }

        let luep = self.calculate_luep()?;
        let prnlos = self.calculate_prnlos()?;
        let mc = self.calculate_mc()?;

        let los_confidence_level = if luep > 0.0 {
            0.0
        } else if prnlos == 0.0 || mc >= 0.9 {
            1.0
        } else {
            1.0 - prnlos
        };

        let rssi = self.calculate_rssi()?;

        Ok(
            RxQuality {
                los_confidence_level,
                rssi,
            }
        )
    }

    /// Finishes receiving and returns to the `Ready` state
    ///
    /// If the receive operation has finished, as indicated by `wait`, this is a
    /// no-op. If the receive operation is still ongoing, it will be aborted.
    pub fn finish_receiving(mut self)
        -> Result<DW1000<SPI, CS, Ready>, (Self, Error<SPI, CS>)>
    {
        if !self.state.finished {
            // Can't use `map_err` and `?` here, as the compiler will complain
            // about `self` moving into the closure.
            match self.force_idle() {
                Ok(())     => (),
                Err(error) => return Err((self, error)),
            }
        }

        Ok(DW1000 {
            ll:    self.ll,
            seq:   self.seq,
            state: Ready,
            max_frame_len: self.max_frame_len,
        })
    }
}

impl<SPI, CS, State> DW1000<SPI, CS, State>
    where
        SPI: spi::Transfer<u8> + spi::Write<u8>,
        CS:  OutputPin,
{
    /// Returns the TX antenna delay
    pub fn get_tx_antenna_delay(&mut self)
        -> Result<Duration, Error<SPI, CS>>
    {
        let tx_antenna_delay = self.ll.tx_antd().read()?.value();

        // Since `tx_antenna_delay` is `u16`, the following will never panic.
        let tx_antenna_delay = Duration::new(tx_antenna_delay.into()).unwrap();

        Ok(tx_antenna_delay)
    }

    /// Returns the network id and address used for sending and receiving
    pub fn get_address(&mut self)
        -> Result<mac::Address, Error<SPI, CS>>
    {
        let panadr = self.ll.panadr().read()?;

        Ok(mac::Address::Short(
            mac::PanId(panadr.pan_id()),
            mac::ShortAddress(panadr.short_addr()),
        ))
    }

    /// Returns the current system time
    pub fn sys_time(&mut self) -> Result<Instant, Error<SPI, CS>> {
        let sys_time = self.ll.sys_time().read()?.value();

        // Since hardware timestamps fit within 40 bits, the following should
        // never panic.
        Ok(Instant::new(sys_time).unwrap())
    }

    /// Provides direct access to the register-level API
    ///
    /// Be aware that by using the register-level API, you can invalidate
    /// various assumptions that the high-level API makes about the operation of
    /// the DW1000. Don't use the register-level and high-level APIs in tandem,
    /// unless you know what you're doing.
    pub fn ll(&mut self) -> &mut ll::DW1000<SPI, CS> {
        &mut self.ll
    }

    /// Force the DW1000 into IDLE mode
    ///
    /// Any ongoing RX/TX operations will be aborted.
    fn force_idle(&mut self)
        -> Result<(), Error<SPI, CS>>
    {
        self.ll.sys_ctrl().write(|w| w.trxoff(0b1))?;
        while self.ll.sys_ctrl().read()?.trxoff() == 0b1 {}

        Ok(())
    }
}

// Can't be derived without putting requirements on `SPI` and `CS`.
impl<SPI, CS, State> fmt::Debug for DW1000<SPI, CS, State>
    where
        State: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "DW1000 {{ state: ")?;
        self.state.fmt(f)?;
        write!(f, ", .. }}")?;

        Ok(())
    }
}

/// An error that can occur when sending or receiving data
pub enum Error<SPI, CS>
    where
        SPI: spi::Transfer<u8> + spi::Write<u8>,
        CS:  OutputPin,
{
    /// Error occured while using SPI bus
    Spi(ll::Error<SPI, CS>),

    /// Receiver FCS error
    Fcs,

    /// PHY header error
    Phy,

    /// Buffer too small
    BufferTooSmall {
        /// Indicates how large a buffer would have been required
        required_len: usize,
    },

    /// Receiver Reed Solomon Frame Sync Loss
    ReedSolomon,

    /// Receiver Frame Wait Timeout
    FrameWaitTimeout,

    /// Receiver Overrun
    Overrun,

    /// Preamble Detection Timeout
    PreambleDetectionTimeout,

    /// Receiver SFD Timeout
    SfdTimeout,

    /// Frame was rejected because due to automatic frame filtering
    ///
    /// It seems that frame filtering is typically handled transparently by the
    /// hardware, and filtered frames aren't usually visible to the driver.
    /// However, sometimes a filtered frame bubbles up and disrupts an ongoing
    /// receive operation, which then causes this error.
    FrameFilteringRejection,

    /// Frame could not be decoded
    Frame(mac::DecodeError),

    /// A delayed frame could not be sent in time
    ///
    /// Please note that the frame was still sent. Replies could still arrive,
    /// and if it was a ranging frame, the resulting range measurement will be
    /// wrong.
    DelayedSendTooLate,

    /// Transmitter could not power up in time for delayed send
    ///
    /// The frame was still transmitted, but the first bytes of the preamble
    /// were likely corrupted.
    DelayedSendPowerUpWarning,

    /// An error occured while serializing or deserializing data
    Ssmarshal(ssmarshal::Error),

    /// The configuration was not valid. Some combinations of settings are not allowed.
    InvalidConfiguration,

    /// The receive operation hasn't finished yet
    RxNotFinished,
}

impl<SPI, CS> From<ll::Error<SPI, CS>> for Error<SPI, CS>
    where
        SPI: spi::Transfer<u8> + spi::Write<u8>,
        CS:  OutputPin,
{
    fn from(error: ll::Error<SPI, CS>) -> Self {
        Error::Spi(error)
    }
}

impl<SPI, CS> From<ssmarshal::Error> for Error<SPI, CS>
    where
        SPI: spi::Transfer<u8> + spi::Write<u8>,
        CS:  OutputPin,
{
    fn from(error: ssmarshal::Error) -> Self {
        Error::Ssmarshal(error)
    }
}

// We can't derive this implementation, as `Debug` is only implemented
// conditionally for `ll::Debug`.
impl<SPI, CS> fmt::Debug for Error<SPI, CS>
    where
        SPI: spi::Transfer<u8> + spi::Write<u8>,
        <SPI as spi::Transfer<u8>>::Error: fmt::Debug,
        <SPI as spi::Write<u8>>::Error: fmt::Debug,
        CS: OutputPin,
        <CS as OutputPin>::Error: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Spi(error) =>
                write!(f, "Spi({:?})", error),
            Error::Fcs =>
                write!(f, "Fcs"),
            Error::Phy =>
                write!(f, "Phy"),
            Error::BufferTooSmall { required_len } =>
                write!(
                    f,
                    "BufferTooSmall {{ required_len: {:?} }}",
                    required_len,
                ),
            Error::ReedSolomon =>
                write!(f, "ReedSolomon"),
            Error::FrameWaitTimeout =>
                write!(f, "FrameWaitTimeout"),
            Error::Overrun =>
                write!(f, "Overrun"),
            Error::PreambleDetectionTimeout =>
                write!(f, "PreambleDetectionTimeout"),
            Error::SfdTimeout =>
                write!(f, "SfdTimeout"),
            Error::FrameFilteringRejection =>
                write!(f, "FrameFilteringRejection"),
            Error::Frame(error) =>
                write!(f, "Frame({:?})", error),
            Error::DelayedSendTooLate =>
                write!(f, "DelayedSendTooLate"),
            Error::DelayedSendPowerUpWarning =>
                write!(f, "DelayedSendPowerUpWarning"),
            Error::Ssmarshal(error) =>
                write!(f, "Ssmarshal({:?})", error),
            Error::InvalidConfiguration =>
                write!(f, "InvalidConfiguration"),
            Error::RxNotFinished =>
                write!(f, "RxNotFinished"),
        }
    }
}


/// Indicates that the `DW1000` instance is not initialized yet
#[derive(Debug)]
pub struct Uninitialized;

/// Indicates that the `DW1000` instance is ready to be used
#[derive(Debug)]
pub struct Ready;

/// Indicates that the `DW1000` instance is currently sending
#[derive(Debug)]
pub struct Sending {
    finished: bool,
}

/// Indicates that the `DW1000` instance is currently receiving
#[derive(Debug)]
pub struct Receiving {
    finished: bool,
    used_config: RxConfig,
}


/// An incoming message
#[derive(Debug)]
pub struct Message<'l> {
    /// The time the message was received
    ///
    /// This time is based on the local system time, as defined in the SYS_TIME
    /// register.
    pub rx_time: Instant,

    /// The MAC frame
    pub frame: mac::Frame<'l>,
}

/// A struct representing the quality of the received message.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct RxQuality {
    /// The confidence that there was Line Of Sight between the sender and the receiver.
    ///
    /// - 0 means it's very unlikely there was LOS.
    /// - 1 means it's very likely there was LOS.
    ///
    /// The number doesn't give a guarantee, but an indication.
    /// It is based on the APS006_Part-3-DW1000-Diagnostics-for-NLOS-Channels-v1.1 document.
    pub los_confidence_level: f32,
    /// The radio signal strength indicator in dBm.
    ///
    /// The value is an estimation that is quite accurate up to -85 dBm.
    /// Above -85 dBm, the estimation underestimates the actual value.
    pub rssi: f32
}
